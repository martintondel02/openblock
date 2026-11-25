//! Client UI model: F3 debug overlay, HUD layout and the tabbed settings screen.
//!
//! This module is deliberately GPU-free: it decides what to show and where,
//! while `ob_render::ui` draws it. Keeping the model separate makes the layout
//! and settings navigation unit-testable without a window or a device.

use std::collections::VecDeque;

use ob_content::{BlockType, Registry};
use ob_core::config::{
    Action, BindKey, CloudQuality, GameMode, GraphicsPreset, KeybindError, Settings, ShadowQuality, TextureStyle,
    WaterQuality,
};
use ob_render::UiFrame;

use crate::crafting::CraftingState;
use crate::inventory::{InventoryLayout, InventoryState, layout as inventory_layout};

/// Number of hotbar slots.
pub const HOTBAR_SLOTS: usize = 9;
/// Default hotbar contents, in slot order.
pub const DEFAULT_HOTBAR: [&str; HOTBAR_SLOTS] = [
    "openblock:grass",
    "openblock:dirt",
    "openblock:stone",
    "openblock:sand",
    "openblock:log",
    "openblock:leaves",
    "openblock:water",
    "openblock:bedrock",
    "openblock:snow",
];
/// Hotbar slot edge length in UI pixels at 1080p.
pub const HOTBAR_SLOT_SIZE: f32 = 44.0;
/// Gap between hotbar slots.
pub const HOTBAR_GAP: f32 = 4.0;
/// Seconds for which the selected block name stays visible after a change.
pub const BLOCK_NAME_SECONDS: f32 = 2.5;
/// Number of heart and hunger icons in the survival HUD.
pub const VITAL_ICON_COUNT: usize = 10;
/// Each vital icon represents this many health or hunger points.
pub const VITAL_POINTS_PER_ICON: f32 = 2.0;
/// Edge length of one vital icon in UI pixels.
pub const VITAL_ICON_SIZE: f32 = 16.0;
/// Gap between vital icons.
pub const VITAL_ICON_GAP: f32 = 2.0;
/// Left margin of the survival vitals rows.
pub const VITAL_MARGIN_X: f32 = 14.0;
/// Number of armor pips drawn next to the hunger row.
pub const ARMOR_ICON_COUNT: usize = 10;
/// Each armor pip represents this many defence points.
pub const ARMOR_POINTS_PER_ICON: f32 = 2.0;
/// Horizontal gap between the hunger row and the armor pips.
pub const ARMOR_ROW_GAP: f32 = 14.0;
/// Seconds the red damage vignette remains visible.
pub const DAMAGE_FLASH_SECONDS: f32 = 0.35;
/// Seconds a pickup or hint toast remains visible.
pub const TOAST_SECONDS: f32 = 2.5;
/// Maximum number of toasts shown at once.
pub const MAX_TOASTS: usize = 3;
/// Width of a death-screen button.
pub const DEATH_BUTTON_WIDTH: f32 = 240.0;
/// Height of a death-screen button.
pub const DEATH_BUTTON_HEIGHT: f32 = 44.0;
/// Vertical gap between death-screen buttons.
pub const DEATH_BUTTON_GAP: f32 = 20.0;
/// Death-screen note explaining that the inventory survives.
pub const DEATH_NOTE: &str = "The world keeps your inventory";

/// Width of the modal settings panel in UI pixels.
pub const SETTINGS_PANEL_WIDTH: f32 = 620.0;
/// Height of the modal settings panel in UI pixels.
pub const SETTINGS_PANEL_HEIGHT: f32 = 520.0;
/// Horizontal inner margin of the settings panel.
pub const SETTINGS_MARGIN: f32 = 28.0;
/// Height of the tab strip.
pub const SETTINGS_TAB_HEIGHT: f32 = 34.0;
/// Top offset of the tab strip below the panel title.
pub const SETTINGS_TAB_TOP: f32 = 72.0;
/// Gap between two settings tabs.
pub const SETTINGS_TAB_GAP: f32 = 8.0;
/// Height of a focusable settings row.
pub const SETTINGS_ROW_HEIGHT: f32 = 34.0;
/// Vertical distance between settings rows; the extra pixels are a visible
/// breathing gap between two focusable rows.
pub const SETTINGS_ROW_STEP: f32 = 44.0;
/// Vertical distance from the panel top to the first settings row.
pub const SETTINGS_CONTENT_TOP: f32 = 132.0;
/// Width of a footer button.
pub const SETTINGS_BUTTON_WIDTH: f32 = 160.0;
/// Height of a footer button.
pub const SETTINGS_BUTTON_HEIGHT: f32 = 38.0;
/// Gap between the two footer buttons.
pub const SETTINGS_BUTTON_GAP: f32 = 24.0;
/// Width of one clickable `<` or `>` adjustment arrow.
pub const SETTINGS_ARROW_WIDTH: f32 = 22.0;
/// Height of one clickable adjustment arrow.
pub const SETTINGS_ARROW_HEIGHT: f32 = 26.0;
/// Horizontal gap between the two adjustment arrows.
pub const SETTINGS_ARROW_GAP: f32 = 10.0;
/// Right margin of the increment arrow inside a row.
pub const SETTINGS_ARROW_RIGHT_MARGIN: f32 = 12.0;
/// Gap between a row value and the decrement arrow.
pub const SETTINGS_VALUE_GAP: f32 = 16.0;
/// Width of the range progress track drawn under the value.
pub const SETTINGS_SLIDER_WIDTH: f32 = 110.0;
/// Gap between the slider track and the decrement arrow.
pub const SETTINGS_SLIDER_GAP: f32 = 16.0;
/// Height of the summary box shown under the Game-tab rows.
pub const SETTINGS_SUMMARY_HEIGHT: f32 = 40.0;
/// Vertical gap between the last row and a summary/source box.
pub const SETTINGS_SUMMARY_GAP: f32 = 16.0;
/// Row pitch of the two-column interactive Controls tab.
pub const SETTINGS_CONTROLS_ROW_STEP: f32 = 28.0;
/// Height of one interactive Controls row.
pub const SETTINGS_CONTROLS_ROW_HEIGHT: f32 = 22.0;
/// Gap between the two Controls columns.
pub const SETTINGS_CONTROLS_COLUMN_GAP: f32 = 12.0;
/// Height of the small type on the Controls tab.
pub const SETTINGS_CONTROLS_TEXT_HEIGHT: f32 = 10.0;
/// Top offset of the footer separator, measured from the panel bottom.
pub const SETTINGS_FOOTER_Y_FROM_BOTTOM: f32 = 70.0;
/// Top offset of the first footer hint line, measured from the panel bottom.
pub const SETTINGS_HINT_Y_FROM_BOTTOM: f32 = 60.0;
/// Vertical distance between footer hint lines.
pub const SETTINGS_HINT_STEP: f32 = 14.0;
/// Smallest render distance the settings screen offers, in chunks.
pub const SETTINGS_MIN_RENDER_DISTANCE: i32 = 2;
/// Largest render distance the settings screen offers, in chunks.
///
/// 32 chunks is 512 blocks. The settings row shows both values because the
/// existing `--render-distance` flag and F3 line are chunk-based.
pub const SETTINGS_MAX_RENDER_DISTANCE: i32 = 32;
/// Smallest field of view the settings screen offers, in degrees.
pub const SETTINGS_MIN_FOV_DEGREES: f32 = 50.0;
/// Largest field of view the settings screen offers, in degrees.
pub const SETTINGS_MAX_FOV_DEGREES: f32 = 110.0;
/// Field-of-view step applied by Left/Right and Enter.
pub const SETTINGS_FOV_STEP: f32 = 5.0;
/// Smallest mouse sensitivity the settings screen offers.
pub const SETTINGS_MIN_MOUSE_SENSITIVITY: f32 = 0.2;
/// Largest mouse sensitivity the settings screen offers.
pub const SETTINGS_MAX_MOUSE_SENSITIVITY: f32 = 3.0;
/// Mouse-sensitivity step applied by Left/Right and Enter.
pub const SETTINGS_SENSITIVITY_STEP: f32 = 0.1;
/// Volume step applied by Left/Right, Enter and the mouse wheel.
pub const SETTINGS_VOLUME_STEP: f32 = 0.05;
/// Footer hint lines, split so they fit the panel at the 8x8 font.
pub const SETTINGS_CONTROLS_HINT_LINES: [&str; 2] = [
    "Enter rebind   Backspace/Delete clear   Reset restores defaults",
    "Tab switch tabs   Up/Down select   Esc back",
];
pub const SETTINGS_HINT_LINES: [&str; 2] = [
    "Tab: switch   Up/Down: select   Left/Right or scroll: change",
    "Click < > to change   Enter: activate   Esc: back",
];
/// Note shown on the Controls tab.
pub const SETTINGS_PERSISTENCE_NOTE: &str = "Settings save automatically";
/// Audio help shown below the Controls reference.
pub const SETTINGS_AUDIO_NOTE: &str = "Audio: Esc > Audio (Sound, Master, SFX, Music)";

/// Clamps a settings volume to `0.0..=1.0`, substituting the default for
/// non-finite persisted values.
fn settings_volume(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

/// Percent label used by the three volume rows.
fn volume_percent(value: f32, fallback: f32) -> String {
    format!("{:.0}%", settings_volume(value, fallback) * 100.0)
}

/// Whether another `delta` step would change a volume row.
fn volume_can_adjust(value: f32, delta: i32, fallback: f32) -> bool {
    let value = settings_volume(value, fallback);
    if delta < 0 { value > 0.0 } else { value < 1.0 }
}

/// Applies one step to a settings volume, rounded to whole percent.
fn adjust_volume(value: &mut f32, delta: i32, fallback: f32) -> bool {
    let current = settings_volume(*value, fallback);
    let next = ((current + SETTINGS_VOLUME_STEP * delta.signum() as f32).clamp(0.0, 1.0) * 100.0).round() / 100.0;
    if (next - *value).abs() > 0.0001 {
        *value = next;
        true
    } else {
        false
    }
}

/// Which screen currently owns input.
///
/// Adding a modal screen (for example a crafting screen) means adding a variant
/// here and extending the matches in the client input handler,
/// `GameState::handle_escape` and `GameState::build_ui_frame_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Normal gameplay with a captured pointer.
    Playing,
    /// Survival inventory / creative palette with a released pointer.
    Inventory,
    /// Full modal settings screen with a released pointer.
    Settings,
    /// Modal crafting screen; gameplay input is blocked.
    Crafting,
    /// Modal furnace screen; gameplay input is blocked while the host is
    /// authoritative for the slots.
    Furnace,
    /// Modal chest screen; gameplay input is blocked.
    Chest,
    /// Modal death screen; gameplay input is blocked.
    Death,
    /// Connecting to a multiplayer server or loading its spawn area.
    ///
    /// The normal single-player client never enters this screen; the network
    /// client uses it for both loading progress and connection failures.
    Connecting,
}

/// One tab of the settings screen, in display order.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    /// Game mode, HUD and debug overlay.
    #[default]
    Game,
    /// Render distance, FOV, sensitivity, fullscreen and mesh workers.
    Display,
    /// Sound on/off and the master/SFX/music volumes.
    Audio,
    /// Interactive action bindings and reset button.
    Controls,
    /// Procedural texture style and external resource pack.
    Textures,
    /// 1.1 visual quality: preset, shadows, clouds, water, bloom and FXAA.
    Graphics,
}

impl SettingsTab {
    /// All tabs, in display order.
    pub const ALL: [SettingsTab; 6] = [
        SettingsTab::Game,
        SettingsTab::Display,
        SettingsTab::Audio,
        SettingsTab::Controls,
        SettingsTab::Textures,
        SettingsTab::Graphics,
    ];

    /// Title drawn on the tab.
    pub fn name(self) -> &'static str {
        match self {
            SettingsTab::Game => "Game",
            SettingsTab::Display => "Display",
            SettingsTab::Audio => "Audio",
            SettingsTab::Controls => "Controls",
            SettingsTab::Textures => "Textures",
            SettingsTab::Graphics => "Graphics",
        }
    }

    /// Parses a tab name case-insensitively (CLI screenshot selection).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tab| tab.name().eq_ignore_ascii_case(name))
    }

    /// Next tab, wrapping from Textures to Game.
    pub fn next(self) -> Self {
        let index = SettingsTab::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        SettingsTab::ALL[(index + 1) % SettingsTab::ALL.len()]
    }

    /// Previous tab, wrapping from Game to Textures.
    pub fn previous(self) -> Self {
        let index = SettingsTab::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        SettingsTab::ALL[(index + SettingsTab::ALL.len() - 1) % SettingsTab::ALL.len()]
    }
}

/// How a settings row is adjusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRowKind {
    /// Bounded numeric value changed in steps.
    Range,
    /// Cyclic choice such as game mode or texture style.
    Choice,
    /// On/off checkbox.
    Toggle,
    /// Row that asks the client to run a side effect (texture pack).
    Action,
    /// Informational row that cannot be changed.
    ReadOnly,
}

/// One focusable row of a settings tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    /// Survival or creative gameplay.
    GameMode,
    /// Whether the HUD is visible.
    ShowHud,
    /// Whether the F3 debug overlay is visible.
    DebugOverlay,
    /// Streaming radius in chunks.
    RenderDistance,
    /// Vertical field of view.
    Fov,
    /// Mouse look multiplier.
    MouseSensitivity,
    /// Fullscreen window toggle.
    Fullscreen,
    /// GUI scale percentage or Auto.
    UiScale,
    /// Read-only worker-thread count.
    MeshWorkers,
    /// Active UI font name and source (read-only).
    UiFont,
    /// Clear the saved UI-font override and use the embedded Monocraft font.
    ResetUiFont,
    /// Re-scan the data/system font folders and apply the chosen font.
    ReloadUiFont,
    /// Procedural texture style.
    TextureStyle,
    /// External resource pack toggle.
    TexturePack,
    /// Global sound on/off.
    AudioEnabled,
    /// Master output volume.
    MasterVolume,
    /// Sound-effect bus volume.
    SfxVolume,
    /// Music and ambience bus volume.
    MusicVolume,
    /// Rebindable action; activating starts key capture.
    Keybind(Action),
    /// Restore every control binding to its factory default.
    ResetKeybinds,
    /// Remove camera shake and the sprint FOV pump.
    ReducedMotion,
    /// Coherent visual-quality preset.
    GraphicsPreset,
    /// Sun-shadow quality.
    ShadowQuality,
    /// Procedural cloud quality.
    Clouds,
    /// Transparent-water quality.
    WaterQuality,
    /// Cloud coverage percentage.
    CloudCoverage,
    /// HDR bloom toggle.
    Bloom,
    /// FXAA-lite toggle.
    Fxaa,
}

impl SettingsRow {
    /// Label drawn on the left of the row.
    pub fn label(self) -> &'static str {
        match self {
            SettingsRow::GameMode => "Game mode",
            SettingsRow::ShowHud => "Show HUD",
            SettingsRow::DebugOverlay => "Debug overlay (F3)",
            SettingsRow::RenderDistance => "Render distance",
            SettingsRow::Fov => "Field of view",
            SettingsRow::MouseSensitivity => "Mouse sensitivity",
            SettingsRow::Fullscreen => "Fullscreen",
            SettingsRow::UiScale => "GUI scale",
            SettingsRow::MeshWorkers => "Mesh workers",
            SettingsRow::UiFont => "UI font",
            SettingsRow::ResetUiFont => "Reset UI font",
            SettingsRow::ReloadUiFont => "Reload UI font",
            SettingsRow::TextureStyle => "Texture style",
            SettingsRow::TexturePack => "Texture pack",
            SettingsRow::AudioEnabled => "Sound",
            SettingsRow::MasterVolume => "Master volume",
            SettingsRow::SfxVolume => "SFX volume",
            SettingsRow::MusicVolume => "Music volume",
            SettingsRow::Keybind(action) => action.label(),
            SettingsRow::ResetKeybinds => "Reset controls",
            SettingsRow::ReducedMotion => "Reduced motion",
            SettingsRow::GraphicsPreset => "Graphics preset",
            SettingsRow::ShadowQuality => "Shadows",
            SettingsRow::Clouds => "Clouds",
            SettingsRow::WaterQuality => "Water",
            SettingsRow::CloudCoverage => "Cloud coverage",
            SettingsRow::Bloom => "Bloom",
            SettingsRow::Fxaa => "FXAA",
        }
    }

    /// How the row reacts to adjustment and drawing.
    pub fn kind(self) -> SettingsRowKind {
        match self {
            SettingsRow::GameMode
            | SettingsRow::TextureStyle
            | SettingsRow::UiScale
            | SettingsRow::GraphicsPreset
            | SettingsRow::ShadowQuality
            | SettingsRow::Clouds
            | SettingsRow::WaterQuality => SettingsRowKind::Choice,
            SettingsRow::ShowHud
            | SettingsRow::DebugOverlay
            | SettingsRow::Fullscreen
            | SettingsRow::AudioEnabled
            | SettingsRow::ReducedMotion
            | SettingsRow::Bloom
            | SettingsRow::Fxaa => SettingsRowKind::Toggle,
            SettingsRow::RenderDistance
            | SettingsRow::Fov
            | SettingsRow::MouseSensitivity
            | SettingsRow::MasterVolume
            | SettingsRow::SfxVolume
            | SettingsRow::MusicVolume
            | SettingsRow::CloudCoverage => SettingsRowKind::Range,
            SettingsRow::TexturePack | SettingsRow::ResetUiFont | SettingsRow::ReloadUiFont => SettingsRowKind::Action,
            SettingsRow::MeshWorkers | SettingsRow::UiFont => SettingsRowKind::ReadOnly,
            SettingsRow::Keybind(_) | SettingsRow::ResetKeybinds => SettingsRowKind::Action,
        }
    }
}

/// A footer button on the settings screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsButton {
    /// Close settings and capture the pointer.
    Resume,
    /// Persist the world and settings, then exit.
    SaveAndQuit,
}

impl SettingsButton {
    /// Label drawn on the button.
    pub fn label(self) -> &'static str {
        match self {
            SettingsButton::Resume => "Resume",
            SettingsButton::SaveAndQuit => "Save & quit",
        }
    }
}

/// Footer buttons in display order.
pub const SETTINGS_BUTTONS: [SettingsButton; 2] = [SettingsButton::Resume, SettingsButton::SaveAndQuit];

/// Action selected on the settings screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    /// Close settings and return to gameplay.
    Resume,
    /// Save and exit the process.
    SaveAndQuit,
    /// A row changed; the client must apply its side effect and persist settings.
    Adjust(SettingsRow),
    /// Enter key capture for an action.
    BeginBind(Action),
    /// Restore every keybinding to its factory default.
    ResetKeybinds,
    /// Clear the persisted UI-font path and restore embedded Monocraft.
    ResetUiFont,
    /// Re-scan the UI-font folders and apply the current choice.
    ReloadUiFont,
}

/// One binding listed by the read-only Controls tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlBinding {
    /// Key or mouse button.
    pub keys: &'static str,
    /// Action performed.
    pub action: &'static str,
}

/// Every binding shown on the Controls tab, in display order.
pub const CONTROLS_REFERENCE: [ControlBinding; 14] = [
    ControlBinding {
        keys: "WASD",
        action: "Move",
    },
    ControlBinding {
        keys: "Space",
        action: "Jump",
    },
    ControlBinding {
        keys: "Shift",
        action: "Crouch / sneak",
    },
    ControlBinding {
        keys: "Ctrl",
        action: "Sprint",
    },
    ControlBinding {
        keys: "F",
        action: "Toggle fly",
    },
    ControlBinding {
        keys: "LMB (hold)",
        action: "Mine",
    },
    ControlBinding {
        keys: "RMB",
        action: "Place / eat / use",
    },
    ControlBinding {
        keys: "1-9 / Wheel",
        action: "Hotbar slot",
    },
    ControlBinding {
        keys: "E",
        action: "Inventory",
    },
    ControlBinding {
        keys: "C",
        action: "Crafting",
    },
    ControlBinding {
        keys: "F3",
        action: "Debug overlay",
    },
    ControlBinding {
        keys: "F1",
        action: "HUD",
    },
    ControlBinding {
        keys: "F2",
        action: "Screenshot",
    },
    ControlBinding {
        keys: "Esc",
        action: "Settings",
    },
];

/// Splits [`CONTROLS_REFERENCE`] into two readable columns.
pub fn controls_reference_columns() -> [Vec<ControlBinding>; 2] {
    let split = CONTROLS_REFERENCE.len().div_ceil(2);
    [
        CONTROLS_REFERENCE[..split].to_vec(),
        CONTROLS_REFERENCE[split..].to_vec(),
    ]
}

/// Runtime values the settings screen shows but does not own.
#[derive(Debug, Clone)]
pub struct SettingsInfo {
    /// Current health in half-hearts.
    pub health: f32,
    /// Current hunger points.
    pub hunger: f32,
    /// Active texture source label, e.g. `painterly` or a pack folder name.
    pub texture_source: String,
    /// Active texture resolution in pixels.
    pub texture_resolution: u32,
    /// Mesh worker-thread count.
    pub mesh_workers: usize,
    /// Effective GUI-scale percentage for the current window/target.
    pub ui_scale_percent: u16,
    /// One-line audio output description, or `off` when unavailable.
    pub audio_status: String,
}

impl Default for SettingsInfo {
    fn default() -> Self {
        Self {
            health: VITAL_ICON_COUNT as f32 * VITAL_POINTS_PER_ICON,
            hunger: VITAL_ICON_COUNT as f32 * VITAL_POINTS_PER_ICON,
            texture_source: "classic".to_string(),
            texture_resolution: 16,
            mesh_workers: 1,
            ui_scale_percent: 100,
            audio_status: "off (headless)".to_string(),
        }
    }
}

/// One row ready to draw: stable metadata plus the current display value.
#[derive(Debug, Clone)]
pub struct SettingsRowInfo {
    /// Row identity.
    pub row: SettingsRow,
    /// Label on the left.
    pub label: &'static str,
    /// Value on the right.
    pub value: String,
    /// Adjustment behaviour.
    pub kind: SettingsRowKind,
    /// Whether the row can be changed.
    pub enabled: bool,
}

/// Fill level of a single heart icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartState {
    /// Both health points for this heart are present.
    Full,
    /// Exactly one of the two health points is present.
    Half,
    /// No health remains in this heart.
    Empty,
}

/// Fill level of a single hunger icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HungerState {
    /// Both hunger points for this drumstick are present.
    Full,
    /// Exactly one of the two hunger points is present.
    Half,
    /// No hunger remains in this drumstick.
    Empty,
}

/// One action offered by the death screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeathAction {
    /// Respawn at the world spawn point and return to gameplay.
    Respawn,
    /// Save the world and exit the process.
    SaveAndQuit,
}

/// Death-screen actions in display order.
pub const DEATH_ACTIONS: [DeathAction; 2] = [DeathAction::Respawn, DeathAction::SaveAndQuit];

/// How urgent a HUD toast is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    /// Normal pickup/hint message.
    Info,
    /// Successful action such as a completed craft.
    Success,
    /// Warning such as a full inventory or a broken tool.
    Warning,
}

/// One fading HUD toast.
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    /// Message text.
    pub text: String,
    /// Remaining lifetime in seconds.
    pub remaining: f32,
    /// Visual urgency.
    pub kind: ToastKind,
}

/// Bounded queue of recent pickup/hint messages.
#[derive(Debug, Clone, Default)]
pub struct ToastQueue {
    entries: VecDeque<Toast>,
}

impl ToastQueue {
    /// Pushes an info toast, dropping the oldest when the queue is full.
    pub fn push(&mut self, text: impl Into<String>) {
        self.push_kind(ToastKind::Info, text);
    }

    /// Pushes a success toast, dropping the oldest when the queue is full.
    pub fn push_success(&mut self, text: impl Into<String>) {
        self.push_kind(ToastKind::Success, text);
    }

    /// Pushes a warning toast, dropping the oldest when the queue is full.
    pub fn push_warning(&mut self, text: impl Into<String>) {
        self.push_kind(ToastKind::Warning, text);
    }

    fn push_kind(&mut self, kind: ToastKind, text: impl Into<String>) {
        while self.entries.len() >= MAX_TOASTS {
            self.entries.pop_front();
        }
        self.entries.push_back(Toast {
            text: text.into(),
            remaining: TOAST_SECONDS,
            kind,
        });
    }

    /// Advances every toast and removes expired entries.
    pub fn tick(&mut self, dt_seconds: f32) {
        if !dt_seconds.is_finite() || dt_seconds <= 0.0 {
            return;
        }
        for toast in &mut self.entries {
            toast.remaining = (toast.remaining - dt_seconds).max(0.0);
        }
        self.entries.retain(|toast| toast.remaining > 0.0);
    }

    /// Number of visible toasts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no toasts are visible.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates oldest to newest.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Toast> {
        self.entries.iter()
    }

    /// Removes every toast.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Tracks health drops and runs the red damage-vignette timer.
#[derive(Debug, Clone, Default)]
pub struct DamageFlash {
    previous_health: Option<f32>,
    remaining: f32,
}

impl DamageFlash {
    /// Observes the current health, starting a flash when it dropped.
    ///
    /// Returns whether a new flash was triggered. The first observation only
    /// records the baseline, so loading a wounded save does not flash.
    pub fn observe(&mut self, health: f32) -> bool {
        let dropped = self
            .previous_health
            .is_some_and(|previous| health.is_finite() && previous.is_finite() && health < previous);
        self.previous_health = Some(health);
        if dropped {
            self.remaining = DAMAGE_FLASH_SECONDS;
        }
        dropped
    }

    /// Starts the damage vignette without a health comparison.
    ///
    /// The network client uses this for authoritative `damage_event` frames,
    /// where health may be restored by the same tick and a drop is not a
    /// reliable signal.
    pub fn trigger(&mut self) {
        self.remaining = DAMAGE_FLASH_SECONDS;
    }

    /// Advances the flash timer.
    pub fn tick(&mut self, dt_seconds: f32) {
        if !dt_seconds.is_finite() || dt_seconds <= 0.0 {
            return;
        }
        self.remaining = (self.remaining - dt_seconds).max(0.0);
    }

    /// Seconds left in the current flash.
    pub fn remaining(&self) -> f32 {
        self.remaining
    }

    /// Vignette alpha in `0.0..=1.0`.
    pub fn alpha(&self) -> f32 {
        (self.remaining / DAMAGE_FLASH_SECONDS).clamp(0.0, 1.0)
    }

    /// Clears the timer and forgets the previous-health baseline.
    pub fn reset(&mut self) {
        self.previous_health = None;
        self.remaining = 0.0;
    }
}

/// A rectangle in UI pixels, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiRect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl UiRect {
    /// Creates a rectangle.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Whether a point lies inside the rectangle.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    /// Centre point of the rectangle.
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Whether this rectangle overlaps another one (touching edges count).
    pub fn intersects(&self, other: Self) -> bool {
        self.x <= other.x + other.w
            && self.x + self.w >= other.x
            && self.y <= other.y + other.h
            && self.y + self.h >= other.y
    }
}

/// Snapshot of runtime statistics shown by the F3 debug overlay.
#[derive(Debug, Clone, Default)]
pub struct DebugStats {
    /// Product version, e.g. `0.0.3`.
    pub version: String,
    /// Current smoothed frames per second.
    pub fps: u32,
    /// Median frame time over the last sampling window in milliseconds.
    pub frame_p50_ms: f32,
    /// 95th percentile frame time in milliseconds.
    pub frame_p95_ms: f32,
    /// Eye position.
    pub position: [f64; 3],
    /// Chunk containing the eye position.
    pub chunk: [i32; 3],
    /// Facing direction label.
    pub facing: &'static str,
    /// Block targeted by the crosshair, if any: resource id, position.
    pub target: Option<(String, [i32; 3])>,
    /// World seed.
    pub seed: u64,
    /// Streaming radius in chunks.
    pub render_distance: i32,
    /// Loaded chunk columns.
    pub loaded_chunks: usize,
    /// Sections drawn after frustum culling.
    pub drawn_sections: usize,
    /// Full-detail sections with a cached GPU mesh.
    pub cached_sections: usize,
    /// Merged far-LOD batches with a cached GPU mesh.
    pub lod_batches: usize,
    /// Shell sections represented by the far batches.
    pub lod_sections: usize,
    /// Sections omitted from far columns by the surface shell.
    pub lod_skipped_sections: usize,
    /// Estimated CPU+GPU bytes held by section and far-batch vertex data.
    pub mesh_bytes: usize,
    /// Triangles drawn.
    pub triangles: u64,
    /// Draw calls issued for the world.
    pub draw_calls: usize,
    /// Mesh worker threads.
    pub mesh_workers: usize,
    /// Mesh jobs currently in flight.
    pub mesh_in_flight: usize,
    /// Mesh jobs completed since start.
    pub mesh_completed: u64,
    /// Dirty sections waiting for a mesh job.
    pub mesh_backlog: usize,
    /// Host heap usage in bytes.
    pub heap_bytes: usize,
    /// Simulation ticks executed.
    pub ticks: u64,
    /// GPU adapter description.
    pub adapter: String,
    /// Window surface size in physical pixels.
    pub window: [u32; 2],
    /// Vertical field of view in degrees.
    pub fov_degrees: f32,
    /// Effective GUI scale line, e.g. `UI 125% (1024x576 logical)`.
    pub ui_scale: String,
    /// Normalised time of day (0 midnight, 0.5 noon).
    pub time_of_day: f32,
    /// Active texture source label, e.g. `painterly 32px` or a pack folder name.
    pub texture_source: String,
    /// Active UI font name reported by the renderer (`Monocraft` or `custom`).
    pub ui_font: String,
    /// `survival` or `creative`.
    pub game_mode: &'static str,
    /// Current health in half-hearts.
    pub health: f32,
    /// Current hunger points.
    pub hunger: f32,
    /// `walk`, `sneak`, `sprint` or `fly`.
    pub movement: &'static str,
    /// Display name of the held item, or `empty`.
    pub held: String,
    /// Remaining and maximum durability of the held tool, if any.
    pub held_durability: Option<(u16, u16)>,
    /// Effective mining speed against the targeted block (1.0 for hand).
    pub mining_speed: f32,
    /// One-line audio output description (`off`, `headless`, or a device).
    pub audio: String,
    /// Optional multiplayer status line (`Net: connected host:port ping Nms ...`).
    pub network: Option<String>,
    /// Total live mobs.
    pub mobs_total: usize,
    /// Live passive mobs.
    pub mobs_passive: usize,
    /// Live hostile mobs.
    pub mobs_hostile: usize,
    /// Day/night phase (`tick_count % 24_000`).
    pub day_phase: u64,
    /// Whether the phase is daylight.
    pub day: bool,
    /// Armor defence points (0 in creative).
    pub defense: u8,
    /// One-line graphics configuration (`High preset | clouds high | ...`).
    pub graphics_summary: String,
    /// Shadow mode, resolution and cascade count (`high 2048² ×3 3×3 PCF`).
    pub shadow_summary: String,
}

impl DebugStats {
    /// Left column lines: identity, performance, position, target and world.
    pub fn left_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!(
                "OpenBlock {} (save format {})",
                self.version,
                ob_core::SAVE_FORMAT_VERSION
            ),
            format!(
                "{} FPS  p50 {:.2} ms  p95 {:.2} ms",
                self.fps, self.frame_p50_ms, self.frame_p95_ms
            ),
            format!(
                "Game: {}",
                if self.game_mode.is_empty() {
                    "survival"
                } else {
                    self.game_mode
                }
            ),
            format!(
                "XYZ {:.3} / {:.3} / {:.3}",
                self.position[0], self.position[1], self.position[2]
            ),
            format!(
                "Vitals: {:.1} / 20 HP, {:.1} / 20 hunger, {} def",
                self.health, self.hunger, self.defense
            ),
            format!(
                "Chunk {} {} {}   Facing {}",
                self.chunk[0], self.chunk[1], self.chunk[2], self.facing
            ),
            format!(
                "Move: {}",
                if self.movement.is_empty() {
                    "walk"
                } else {
                    self.movement
                }
            ),
            self.held_line(),
            format!(
                "FOV {:.0}   Render distance {} chunks / {} blocks",
                self.fov_degrees,
                self.render_distance,
                self.render_distance * 16
            ),
            format!(
                "Textures: {}",
                if self.texture_source.is_empty() {
                    "built-in"
                } else {
                    &self.texture_source
                }
            ),
            format!(
                "Seed {}   {}",
                self.seed,
                if self.ui_scale.is_empty() {
                    "UI scale ?"
                } else {
                    &self.ui_scale
                }
            ),
            format!(
                "Chunks {}  Drawn {}/{}  Draws {}  LOD {}",
                self.loaded_chunks, self.drawn_sections, self.cached_sections, self.draw_calls, self.lod_batches
            ),
            format!(
                "Tri {}  Verts {}  Far {}s/{}skip",
                self.triangles,
                self.triangles.saturating_mul(3),
                self.lod_sections,
                self.lod_skipped_sections
            ),
            match &self.target {
                Some((id, pos)) => {
                    format!("Target {} at {} {} {}", id, pos[0], pos[1], pos[2])
                }
                None => "Target none".to_string(),
            },
            format!(
                "Mesh workers {}   in flight {}   backlog {}   done {}",
                self.mesh_workers, self.mesh_in_flight, self.mesh_backlog, self.mesh_completed
            ),
            format!(
                "Heap {:.1} MiB  Mesh {:.1} MiB  Ticks {}  Mobs {}",
                self.heap_bytes as f64 / (1024.0 * 1024.0),
                self.mesh_bytes as f64 / (1024.0 * 1024.0),
                self.ticks,
                self.mobs_total,
            ),
        ];
        if lines.len() < 15 {
            lines.resize(15, String::new());
        }
        lines
    }

    /// `Held: Stone Pickaxe 129/131  speed 4.0` line shown in the left column.
    pub fn held_line(&self) -> String {
        let held = if self.held.is_empty() { "empty" } else { &self.held };
        let durability = self
            .held_durability
            .map(|(remaining, max)| format!(" {remaining}/{max}"))
            .unwrap_or_default();
        format!("Held: {held}{durability}  speed {:.1}", self.mining_speed)
    }

    /// Right column lines: hardware and window information.
    ///
    /// Long adapter descriptions (Mesa reports the CPU as well as the GPU) are
    /// wrapped so the secondary column stays narrow enough to sit beside the
    /// left column at 1280x720. Every character is preserved.
    pub fn right_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(network) = &self.network {
            lines.extend(wrap_words(network, DEBUG_RIGHT_WRAP_COLUMNS));
        }
        for text in [
            format!("GPU {}", self.adapter),
            format!("Window {}x{}", self.window[0], self.window[1]),
            "Renderer wgpu (Vulkan)".to_string(),
            "Mesh pipeline: greedy + AO".to_string(),
            format!("Audio {}", if self.audio.is_empty() { "off" } else { &self.audio }),
            format!(
                "UI font: {}",
                if self.ui_font.is_empty() {
                    ob_render::default_ui_font_name()
                } else {
                    &self.ui_font
                }
            ),
            format!(
                "Graphics {}",
                if self.graphics_summary.is_empty() {
                    "preset ?"
                } else {
                    &self.graphics_summary
                }
            ),
            format!(
                "Shadows {}",
                if self.shadow_summary.is_empty() {
                    "off"
                } else {
                    &self.shadow_summary
                }
            ),
        ] {
            lines.extend(wrap_words(&text, DEBUG_RIGHT_WRAP_COLUMNS));
        }
        lines
    }
}

impl DebugStats {
    /// Reduced left-column set used when the logical target is short.
    ///
    /// At 960x540 with 150% GUI scale the logical target is 640x360. The full
    /// 16-line F3 column cannot fit above the hotbar there, so the overlay
    /// keeps identity, performance, position, vitals, world, UI scale and
    /// target information readable instead of spilling over the HUD.
    pub fn compact_left_lines(&self) -> Vec<String> {
        vec![
            format!(
                "OpenBlock {} (save format {})",
                self.version,
                ob_core::SAVE_FORMAT_VERSION
            ),
            format!(
                "{} FPS  p50 {:.2} ms  p95 {:.2} ms",
                self.fps, self.frame_p50_ms, self.frame_p95_ms
            ),
            format!(
                "Game: {}   Move: {}   UI font: {}",
                if self.game_mode.is_empty() {
                    "survival"
                } else {
                    self.game_mode
                },
                if self.movement.is_empty() {
                    "walk"
                } else {
                    self.movement
                },
                if self.ui_font.is_empty() {
                    ob_render::default_ui_font_name()
                } else {
                    &self.ui_font
                }
            ),
            format!(
                "XYZ {:.3} / {:.3} / {:.3}   Chunk {} {} {}",
                self.position[0], self.position[1], self.position[2], self.chunk[0], self.chunk[1], self.chunk[2]
            ),
            format!(
                "Vitals: {:.1} / 20 HP, {:.1} / 20 hunger, {} def",
                self.health, self.hunger, self.defense
            ),
            format!(
                "FOV {:.0}   Render distance {} chunks / {} blocks   Facing {}",
                self.fov_degrees,
                self.render_distance,
                self.render_distance * 16,
                self.facing
            ),
            format!(
                "Seed {}   {}",
                self.seed,
                if self.ui_scale.is_empty() {
                    "UI scale ?"
                } else {
                    &self.ui_scale
                }
            ),
            format!(
                "Chunks {}  Sec {}/{}  Draws {}  LOD {}  Tri {}",
                self.loaded_chunks,
                self.drawn_sections,
                self.cached_sections,
                self.draw_calls,
                self.lod_batches,
                self.triangles
            ),
            format!(
                "Mesh {}w {}in {}done   Mobs {}   Heap {:.1} MiB   Mesh {:.1} MiB",
                self.mesh_workers,
                self.mesh_in_flight,
                self.mesh_completed,
                self.mobs_total,
                self.heap_bytes as f64 / (1024.0 * 1024.0),
                self.mesh_bytes as f64 / (1024.0 * 1024.0)
            ),
            match &self.target {
                Some((id, pos)) => format!(
                    "Held: {}   Target {} at {} {} {}",
                    if self.held.is_empty() { "empty" } else { &self.held },
                    id,
                    pos[0],
                    pos[1],
                    pos[2]
                ),
                None => format!(
                    "Held: {}   Target none",
                    if self.held.is_empty() { "empty" } else { &self.held }
                ),
            },
        ]
    }
}

/// Maximum number of characters per line in the F3 right column.
pub const DEBUG_RIGHT_WRAP_COLUMNS: usize = 34;

/// Wraps `text` at spaces, keeping every character (ASCII-only input in the
/// overlay). Words longer than `max_chars` are split so wrapping terminates.
fn wrap_words(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        if word.is_empty() {
            if !current.is_empty() {
                current.push(' ');
            }
            continue;
        }
        let mut word = word.to_string();
        loop {
            let extra = usize::from(!current.is_empty());
            if current.chars().count() + extra + word.chars().count() <= max_chars {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(&word);
                break;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                continue;
            }
            // A single word is longer than the column: hard-split it.
            let mut chunk = String::new();
            let mut rest = String::new();
            for (index, character) in word.chars().enumerate() {
                if index < max_chars {
                    chunk.push(character);
                } else {
                    rest.push(character);
                }
            }
            lines.push(chunk);
            if rest.is_empty() {
                break;
            }
            word = rest;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Glyph scale of the F3 overlay's left column.
pub const DEBUG_TEXT_SCALE: f32 = 1.8;
/// Glyph scale of the dimmer hardware column.
pub const DEBUG_SECONDARY_SCALE: f32 = 1.6;
/// Extra leading between two F3 text lines, in addition to the glyph height.
///
/// The old overlay drew 16 px glyphs on a 16 px line step, so consecutive lines
/// touched. A minimum of six extra pixels keeps descenders apart; eight is used.
pub const DEBUG_LINE_GAP: f32 = 8.0;
/// Extra vertical gap between two F3 sections.
pub const DEBUG_SECTION_GAP: f32 = 10.0;
/// Inner padding of an F3 column panel.
pub const DEBUG_PADDING: f32 = 14.0;
/// Horizontal gap between the left and right F3 panels.
pub const DEBUG_COLUMN_GAP: f32 = 24.0;
/// Vertical gap between the two panels when they are stacked.
pub const DEBUG_PANEL_GAP: f32 = 12.0;
/// Margin between the F3 panels and the screen edge.
pub const DEBUG_MARGIN: f32 = 8.0;
/// Space kept free at the bottom of the screen for the control hint.
pub const DEBUG_FOOTER_SPACE: f32 = 30.0;
/// Scale of the footer hint line.
pub const DEBUG_FOOTER_SCALE: f32 = 1.2;
/// Candidate glyph scales, largest first, used to fit narrow screens.
pub const DEBUG_SCALES: [f32; 9] = [1.8, 1.7, 1.6, 1.5, 1.4, 1.3, 1.2, 1.1, 1.0];
/// Section index of every line returned by [`DebugStats::left_lines`].
pub const DEBUG_LEFT_SECTIONS: [usize; 16] = [0, 0, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 3, 3];
/// Section index of every line returned by [`DebugStats::compact_left_lines`].
pub const DEBUG_COMPACT_SECTIONS: [usize; 10] = [0, 0, 1, 1, 1, 1, 2, 2, 2, 3];
/// Maximum glyph scales tried for the compact F3 overlay, largest first.
pub const DEBUG_COMPACT_SCALES: [f32; 6] = [1.4, 1.3, 1.2, 1.1, 1.0, 0.9];
/// Logical height below which the F3 overlay uses its reduced line set.
pub const DEBUG_COMPACT_MAX_HEIGHT: f32 = 420.0;

/// How the two F3 columns are arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugColumns {
    /// Hardware column to the right of the stats column.
    SideBySide,
    /// Hardware column below the stats column on narrow screens.
    Stacked,
    /// Hardware column omitted because even the stacked form does not fit.
    LeftOnly,
}

/// Placement of one F3 text line inside its column panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DebugLinePlacement {
    /// Index into the source line list.
    pub index: usize,
    /// Section index used for grouping.
    pub section: usize,
    /// Left edge of the glyphs.
    pub x: f32,
    /// Top edge of the glyphs.
    pub y: f32,
    /// Measured advance width at the column scale.
    pub width: f32,
}

/// One F3 column with its panel background and line placement.
#[derive(Debug, Clone, PartialEq)]
pub struct DebugColumnLayout {
    /// Panel background rectangle.
    pub panel: UiRect,
    /// Glyph scale used by the column.
    pub scale: f32,
    /// Placed lines, top to bottom.
    pub lines: Vec<DebugLinePlacement>,
}

/// Pure geometry of the F3 overlay for a target size.
#[derive(Debug, Clone, PartialEq)]
pub struct DebugOverlayLayout {
    /// Arrangement of the two columns.
    pub columns: DebugColumns,
    /// Left statistics column, always present.
    pub left: DebugColumnLayout,
    /// Right hardware column, omitted only on very small targets.
    pub right: Option<DebugColumnLayout>,
    /// Y of the separator drawn under the header line.
    pub header_separator_y: f32,
    /// Y of the footer hint.
    pub footer_y: f32,
    /// Whether the reduced compact line set is in use (small logical targets).
    pub compact: bool,
}

/// Line step of the F3 overlay at `scale`, including the breathing gap.
pub fn debug_line_step(scale: f32) -> f32 {
    ob_render::line_height(scale) + DEBUG_LINE_GAP
}

/// Height of a column panel for `line_count` lines split into `sections`.
fn debug_panel_height(line_count: usize, section_count: usize, scale: f32) -> f32 {
    if line_count == 0 {
        return DEBUG_PADDING * 2.0;
    }
    line_count as f32 * ob_render::line_height(scale)
        + line_count.saturating_sub(1) as f32 * DEBUG_LINE_GAP
        + section_count.saturating_sub(1) as f32 * DEBUG_SECTION_GAP
        + DEBUG_PADDING * 2.0
}

/// Places one column starting at `top_left`.
fn debug_column(
    frame: &UiFrame,
    lines: &[String],
    sections: &[usize],
    scale: f32,
    top_left: (f32, f32),
    max_width: f32,
) -> DebugColumnLayout {
    let mut placements = Vec::with_capacity(lines.len());
    let mut y = top_left.1 + DEBUG_PADDING;
    let mut previous_section = None;
    let mut widest = 0.0_f32;
    for (index, text) in lines.iter().enumerate() {
        let section = sections.get(index).copied().unwrap_or(0);
        if previous_section.is_some_and(|previous| previous != section) {
            y += DEBUG_SECTION_GAP;
        }
        previous_section = Some(section);
        let line_width = frame.text_width(scale, text);
        widest = widest.max(line_width);
        placements.push(DebugLinePlacement {
            index,
            section,
            x: top_left.0 + DEBUG_PADDING,
            y,
            width: line_width,
        });
        y += debug_line_step(scale);
    }
    let panel_height = debug_panel_height(lines.len(), debug_section_count(sections), scale);
    let panel_width = (widest + DEBUG_PADDING * 2.0).min(max_width);
    DebugColumnLayout {
        panel: UiRect::new(top_left.0, top_left.1, panel_width, panel_height),
        scale,
        lines: placements,
    }
}

/// Attempts one column arrangement at `scale`, returning it when it fits.
fn debug_layout_at(
    screen: [f32; 2],
    left_lines: &[String],
    right_lines: &[String],
    scale: f32,
) -> Option<DebugOverlayLayout> {
    let measure = UiFrame::new(screen[0], screen[1]);
    let right_scale = (scale - 0.2).max(1.0);
    let available_width = (screen[0] - DEBUG_MARGIN * 2.0).max(1.0);
    let available_height = (screen[1] - DEBUG_MARGIN * 2.0 - DEBUG_FOOTER_SPACE).max(1.0);
    let left_width = left_lines
        .iter()
        .map(|line| measure.text_width(scale, line))
        .fold(0.0_f32, f32::max);
    let right_width = right_lines
        .iter()
        .map(|line| measure.text_width(right_scale, line))
        .fold(0.0_f32, f32::max);
    let left_height = debug_panel_height(left_lines.len(), debug_section_count(&DEBUG_LEFT_SECTIONS), scale);
    let right_height = debug_panel_height(right_lines.len(), 1, right_scale);
    let right_sections = vec![0; right_lines.len()];

    let fit_columns = |left: DebugColumnLayout, right: Option<DebugColumnLayout>, columns| {
        let panel_bottom = right.as_ref().map_or(left.panel.y + left.panel.h, |right| {
            (left.panel.y + left.panel.h).max(right.panel.y + right.panel.h)
        });
        // The control hint sits just below the overlay panels so it never lands
        // on top of the hotbar (which is centred at the bottom of the screen).
        let footer_y = panel_bottom + 6.0;
        let footer_bottom = footer_y + ob_render::line_height(DEBUG_FOOTER_SCALE);
        let hotbar_top = screen[1] - HOTBAR_SLOT_SIZE - 18.0;
        (panel_bottom <= available_height + DEBUG_MARGIN && footer_bottom <= hotbar_top - 4.0).then_some(
            DebugOverlayLayout {
                compact: false,
                columns,
                header_separator_y: left
                    .lines
                    .first()
                    .map_or(DEBUG_MARGIN, |line| line.y + ob_render::line_height(scale) + 2.0),
                left,
                right,
                footer_y,
            },
        )
    };

    let side_by_side_width = left_width + DEBUG_PADDING * 2.0 + DEBUG_COLUMN_GAP + right_width + DEBUG_PADDING * 2.0;
    if side_by_side_width <= available_width && left_height <= available_height {
        let left = debug_column(
            &measure,
            left_lines,
            &DEBUG_LEFT_SECTIONS,
            scale,
            (DEBUG_MARGIN, DEBUG_MARGIN),
            available_width,
        );
        let right = debug_column(
            &measure,
            right_lines,
            &right_sections,
            right_scale,
            (DEBUG_MARGIN + left.panel.w + DEBUG_COLUMN_GAP, DEBUG_MARGIN),
            available_width,
        );
        return fit_columns(left, Some(right), DebugColumns::SideBySide);
    }
    if left_height + DEBUG_PANEL_GAP + right_height <= available_height {
        let left = debug_column(
            &measure,
            left_lines,
            &DEBUG_LEFT_SECTIONS,
            scale,
            (DEBUG_MARGIN, DEBUG_MARGIN),
            available_width,
        );
        let right = debug_column(
            &measure,
            right_lines,
            &right_sections,
            right_scale,
            (DEBUG_MARGIN, DEBUG_MARGIN + left.panel.h + DEBUG_PANEL_GAP),
            available_width,
        );
        return fit_columns(left, Some(right), DebugColumns::Stacked);
    }
    None
}

/// Number of distinct section indices in `sections`.
fn debug_section_count(sections: &[usize]) -> usize {
    let mut count = 0;
    let mut previous = None;
    for section in sections {
        if previous != Some(*section) {
            count += 1;
            previous = Some(*section);
        }
    }
    count
}

/// Builds the reduced F3 layout used on short logical targets.
fn debug_compact_layout(screen: [f32; 2], stats: &DebugStats) -> DebugOverlayLayout {
    let lines = stats.compact_left_lines();
    let measure = UiFrame::new(screen[0], screen[1]);
    let available_width = (screen[0] - DEBUG_MARGIN * 2.0).max(1.0);
    let hotbar_top = screen[1] - HOTBAR_SLOT_SIZE - 18.0;
    let mut layout = None;
    for scale in DEBUG_COMPACT_SCALES {
        let left = debug_column(
            &measure,
            &lines,
            &DEBUG_COMPACT_SECTIONS,
            scale,
            (DEBUG_MARGIN, DEBUG_MARGIN),
            available_width,
        );
        let footer_y = left.panel.y + left.panel.h + 6.0;
        let footer_bottom = footer_y + ob_render::line_height(DEBUG_FOOTER_SCALE);
        if left.panel.y + left.panel.h <= screen[1] - DEBUG_MARGIN && footer_bottom <= hotbar_top - 4.0 {
            layout = Some(DebugOverlayLayout {
                compact: true,
                columns: DebugColumns::LeftOnly,
                header_separator_y: left
                    .lines
                    .first()
                    .map_or(DEBUG_MARGIN, |line| line.y + ob_render::line_height(scale) + 2.0),
                left,
                right: None,
                footer_y,
            });
            break;
        }
    }
    layout.unwrap_or_else(|| {
        let scale = *DEBUG_COMPACT_SCALES.last().unwrap_or(&1.0);
        let left = debug_column(
            &measure,
            &lines,
            &DEBUG_COMPACT_SECTIONS,
            scale,
            (DEBUG_MARGIN, DEBUG_MARGIN),
            available_width,
        );
        DebugOverlayLayout {
            compact: true,
            columns: DebugColumns::LeftOnly,
            header_separator_y: left
                .lines
                .first()
                .map_or(DEBUG_MARGIN, |line| line.y + ob_render::line_height(scale) + 2.0),
            footer_y: left.panel.y + left.panel.h + 6.0,
            left,
            right: None,
        }
    })
}

/// Computes the F3 overlay geometry for a deterministic stats snapshot.
///
/// The largest scale that keeps both columns (and the footer) on screen is
/// chosen. When the columns do not fit side by side, the hardware column is
/// stacked below the stats column; on very small targets it is omitted so the
/// remaining text stays readable.
pub fn debug_overlay_layout(screen: [f32; 2], stats: &DebugStats) -> DebugOverlayLayout {
    if screen[1] < DEBUG_COMPACT_MAX_HEIGHT {
        return debug_compact_layout(screen, stats);
    }
    let left_lines = stats.left_lines();
    let right_lines = stats.right_lines();
    for scale in DEBUG_SCALES {
        if let Some(layout) = debug_layout_at(screen, &left_lines, &right_lines, scale) {
            return layout;
        }
    }
    // The smallest scale still does not fit: omit the secondary column and
    // keep the most important information readable.
    let scale = DEBUG_SCALES[DEBUG_SCALES.len() - 1];
    let measure = UiFrame::new(screen[0], screen[1]);
    let available_width = (screen[0] - DEBUG_MARGIN * 2.0).max(1.0);
    let left = debug_column(
        &measure,
        &left_lines,
        &DEBUG_LEFT_SECTIONS,
        scale,
        (DEBUG_MARGIN, DEBUG_MARGIN),
        available_width,
    );
    DebugOverlayLayout {
        compact: false,
        columns: DebugColumns::LeftOnly,
        header_separator_y: left
            .lines
            .first()
            .map_or(DEBUG_MARGIN, |line| line.y + ob_render::line_height(scale) + 2.0),
        footer_y: left.panel.y + left.panel.h + 6.0,
        left,
        right: None,
    }
}

/// UI state that survives across frames.
#[derive(Debug, Clone)]
pub struct UiState {
    /// Current screen.
    pub screen: Screen,
    /// Whether the F3 overlay is visible.
    pub debug_overlay: bool,
    /// Whether the HUD (crosshair, hotbar) is visible.
    pub hud_visible: bool,
    /// Current persisted settings.
    pub settings: Settings,
    /// Effective GUI-scale percentage for the current physical target.
    ///
    /// Persisted [`Settings::ui_scale_percent`] may be `Auto`; the runtimes
    /// update this resolved value whenever the window size or setting changes.
    pub ui_scale_percent: u16,
    /// Active tab of the settings screen.
    pub settings_tab: SettingsTab,
    /// Flat keyboard focus on the settings screen: tab rows first, then the
    /// footer buttons in [`SETTINGS_BUTTONS`] order.
    pub settings_selected_row: usize,
    /// Hovered settings tab, if any.
    pub settings_hovered_tab: Option<SettingsTab>,
    /// Hovered settings row index within the active tab, if any.
    pub settings_hovered_row: Option<usize>,
    /// Hovered footer button index, if any.
    pub settings_hovered_button: Option<usize>,
    /// Hotbar slot selected.
    pub hotbar_selected: usize,
    /// Resource ids of the blocks in each hotbar slot.
    pub hotbar: [&'static str; HOTBAR_SLOTS],
    /// Inventory selection, hover and scroll.
    pub inventory: InventoryState,
    /// Crafting screen selection, hover and scroll.
    pub crafting: CraftingState,
    /// Remaining seconds to show the selected block name.
    pub block_name_seconds: f32,
    /// Recently broken block name shown briefly near the crosshair.
    pub break_flash: Option<String>,
    /// Remaining seconds to show [`Self::break_flash`].
    pub break_flash_seconds: f32,
    /// Cursor position in UI pixels.
    pub cursor: (f32, f32),
    /// Display name of the active external resource pack, if one is loaded.
    pub active_texture_pack: Option<String>,
    /// Active UI font name reported by the renderer (`Monocraft` or `custom`).
    pub active_ui_font_name: String,
    /// Source of the active UI font: `built-in` or the font file's base name.
    pub active_ui_font_source: String,
    /// Drag value for a future settings slider.
    pub dragging: bool,
    /// Recent pickup/hint messages shown above the vitals.
    pub toasts: ToastQueue,
    /// Keyboard focus on the death screen (0 = respawn, 1 = save and quit).
    pub death_selected: usize,
    /// Hovered death-screen button, if any.
    pub death_hovered: Option<usize>,
    /// Remaining seconds of the crosshair hit marker after a mob hit.
    pub hit_marker_seconds: f32,
    /// Optional cause hint shown on the death screen.
    pub death_cause: Option<String>,
    /// Settings opened without a loaded world (the title screen). The Game
    /// mode row is disabled and explained instead of silently changing a host.
    pub host_optional: bool,
    /// Action currently waiting for a key or mouse-button capture.
    pub bind_capture: Option<Action>,
    /// Feedback shown below the Controls rows.
    pub binding_message: Option<String>,
    /// Whether [`Self::binding_message`] reports an error (red) or success.
    pub binding_message_error: bool,
    /// Whether a text field currently owns the keyboard. Keybind capture is
    /// refused while this is set, so typing a world name or chat cannot rebind.
    pub text_input_active: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            screen: Screen::Playing,
            debug_overlay: false,
            hud_visible: true,
            settings: Settings::default(),
            ui_scale_percent: 100,
            settings_tab: SettingsTab::Game,
            settings_selected_row: 0,
            settings_hovered_tab: None,
            settings_hovered_row: None,
            settings_hovered_button: None,
            hotbar_selected: 0,
            hotbar: DEFAULT_HOTBAR,
            inventory: InventoryState::default(),
            crafting: CraftingState::default(),
            block_name_seconds: 0.0,
            break_flash: None,
            break_flash_seconds: 0.0,
            cursor: (0.0, 0.0),
            active_texture_pack: None,
            active_ui_font_name: "Monocraft".to_string(),
            active_ui_font_source: "built-in".to_string(),
            dragging: false,
            toasts: ToastQueue::default(),
            death_selected: 0,
            death_hovered: None,
            hit_marker_seconds: 0.0,
            death_cause: None,
            host_optional: false,
            bind_capture: None,
            binding_message: None,
            binding_message_error: false,
            text_input_active: false,
        }
    }
}

impl UiState {
    /// Creates state seeded from persisted settings.
    pub fn from_settings(mut settings: Settings) -> Self {
        settings.clamp();
        Self {
            debug_overlay: settings.show_debug_overlay,
            hud_visible: settings.show_hud,
            ui_scale_percent: 100,
            active_ui_font_name: ob_render::default_ui_font_name().to_string(),
            active_ui_font_source: "built-in".to_string(),
            settings,
            ..Self::default()
        }
    }

    /// Advances timers.
    pub fn tick(&mut self, dt_seconds: f32) {
        self.block_name_seconds = (self.block_name_seconds - dt_seconds).max(0.0);
        self.hit_marker_seconds = (self.hit_marker_seconds - dt_seconds).max(0.0);
        self.break_flash_seconds = (self.break_flash_seconds - dt_seconds).max(0.0);
        if self.break_flash_seconds <= 0.0 {
            self.break_flash = None;
        }
        self.toasts.tick(dt_seconds);
    }

    /// Toggles the F3 overlay and mirrors it into settings.
    pub fn toggle_debug_overlay(&mut self) {
        self.debug_overlay = !self.debug_overlay;
        self.settings.show_debug_overlay = self.debug_overlay;
    }

    /// Toggles the HUD and mirrors it into settings.
    pub fn toggle_hud(&mut self) {
        self.hud_visible = !self.hud_visible;
        self.settings.show_hud = self.hud_visible;
    }

    /// Selects a hotbar slot, showing the block name.
    pub fn select_hotbar(&mut self, slot: usize) {
        self.hotbar_selected = slot % HOTBAR_SLOTS;
        self.block_name_seconds = BLOCK_NAME_SECONDS;
    }

    /// Cycles the hotbar selection by `steps` slots (normally the mouse wheel).
    pub fn cycle_hotbar(&mut self, steps: i32) {
        let slot = (self.hotbar_selected as i32 + steps).rem_euclid(HOTBAR_SLOTS as i32);
        self.select_hotbar(slot as usize);
    }

    /// Block type stored in a hotbar slot, or `None` when the current registry
    /// does not contain it (so the HUD can skip slots that do not exist).
    pub fn hotbar_block<'a>(&self, registry: &'a Registry, index: usize) -> Option<&'a BlockType> {
        let name = *self.hotbar.get(index)?;
        registry.id(name).map(|id| registry.block(id))
    }

    /// Short display name of a hotbar slot's block, preserving the legacy
    /// lowercase HUD label (the inventory uses title-cased display names).
    pub fn hotbar_label(&self, index: usize) -> Option<&'static str> {
        let name = *self.hotbar.get(index)?;
        Some(name.split_once(':').map(|(_, label)| label).unwrap_or(name))
    }

    /// Opens the inventory, releasing the pointer.
    pub fn open_inventory(&mut self) {
        self.screen = Screen::Inventory;
        self.inventory.open();
    }

    /// Closes the inventory and returns to gameplay.
    pub fn close_inventory(&mut self) {
        if self.screen == Screen::Inventory {
            self.screen = Screen::Playing;
        }
    }

    /// Toggles the inventory from gameplay or closes it again.
    pub fn toggle_inventory(&mut self) {
        match self.screen {
            Screen::Playing => self.open_inventory(),
            Screen::Inventory => self.close_inventory(),
            Screen::Settings
            | Screen::Crafting
            | Screen::Furnace
            | Screen::Chest
            | Screen::Death
            | Screen::Connecting => {}
        }
    }

    /// Opens the crafting screen from gameplay.
    pub fn open_crafting(&mut self) {
        if self.screen != Screen::Playing {
            return;
        }
        self.crafting.open();
        self.screen = Screen::Crafting;
        self.settings_clear_hover();
    }

    /// Closes the crafting screen and returns to gameplay.
    pub fn close_crafting(&mut self) {
        if self.screen == Screen::Crafting {
            self.screen = Screen::Playing;
        }
    }

    /// Toggles crafting from gameplay or closes it again.
    pub fn toggle_crafting(&mut self) {
        match self.screen {
            Screen::Playing => self.open_crafting(),
            Screen::Crafting => self.close_crafting(),
            Screen::Inventory
            | Screen::Settings
            | Screen::Furnace
            | Screen::Chest
            | Screen::Death
            | Screen::Connecting => {}
        }
    }

    /// Opens the furnace modal screen from gameplay.
    pub fn open_furnace(&mut self) {
        if self.screen != Screen::Playing {
            return;
        }
        self.screen = Screen::Furnace;
        self.settings_clear_hover();
    }

    /// Opens the chest modal screen from gameplay.
    pub fn open_chest(&mut self) {
        if self.screen != Screen::Playing {
            return;
        }
        self.screen = Screen::Chest;
        self.settings_clear_hover();
    }

    /// Leaves a furnace or chest screen and returns to gameplay.
    pub fn close_container(&mut self) {
        if matches!(self.screen, Screen::Furnace | Screen::Chest) {
            self.screen = Screen::Playing;
        }
    }

    /// Whether a container screen currently owns input.
    pub fn container_open(&self) -> bool {
        matches!(self.screen, Screen::Furnace | Screen::Chest)
    }

    /// Opens the full settings screen directly from gameplay.
    ///
    /// Esc reaches this from [`Screen::Playing`]; the death screen keeps
    /// priority, so opening settings from any other screen is a no-op.
    pub fn open_settings(&mut self) {
        if self.screen != Screen::Playing {
            return;
        }
        self.screen = Screen::Settings;
        self.settings_tab = SettingsTab::Game;
        self.settings_selected_row = 0;
        self.settings_clear_hover();
        self.cancel_bind();
        self.text_input_active = false;
    }

    /// Closes settings and returns to gameplay.
    pub fn close_settings(&mut self) {
        if self.screen == Screen::Settings {
            self.screen = Screen::Playing;
        }
        self.settings_clear_hover();
        self.cancel_bind();
    }

    /// Closes menus and returns to gameplay.
    pub fn resume(&mut self) {
        self.screen = Screen::Playing;
        self.death_cause = None;
        self.settings_clear_hover();
        self.cancel_bind();
    }

    /// Switches to a settings tab and focuses its first row.
    pub fn settings_select_tab(&mut self, tab: SettingsTab) {
        self.settings_tab = tab;
        self.settings_selected_row = 0;
        self.settings_clear_hover();
        self.cancel_bind();
    }

    /// Advances to the next settings tab, wrapping around.
    pub fn settings_tab_next(&mut self) {
        self.settings_select_tab(self.settings_tab.next());
    }

    /// Moves to the previous settings tab, wrapping around.
    pub fn settings_tab_prev(&mut self) {
        self.settings_select_tab(self.settings_tab.previous());
    }

    /// Rows belonging to the active tab, in display order.
    pub fn settings_rows(&self) -> Vec<SettingsRow> {
        match self.settings_tab {
            SettingsTab::Game => vec![SettingsRow::GameMode, SettingsRow::ShowHud, SettingsRow::DebugOverlay],
            SettingsTab::Display => vec![
                SettingsRow::RenderDistance,
                SettingsRow::Fov,
                SettingsRow::MouseSensitivity,
                SettingsRow::Fullscreen,
                SettingsRow::UiScale,
                SettingsRow::ReducedMotion,
                SettingsRow::MeshWorkers,
                SettingsRow::UiFont,
                SettingsRow::ResetUiFont,
                SettingsRow::ReloadUiFont,
            ],
            SettingsTab::Audio => vec![
                SettingsRow::AudioEnabled,
                SettingsRow::MasterVolume,
                SettingsRow::SfxVolume,
                SettingsRow::MusicVolume,
            ],
            SettingsTab::Controls => {
                let mut rows: Vec<SettingsRow> = Action::ALL.into_iter().map(SettingsRow::Keybind).collect();
                rows.push(SettingsRow::ResetKeybinds);
                rows
            }
            SettingsTab::Textures => vec![SettingsRow::TextureStyle, SettingsRow::TexturePack],
            SettingsTab::Graphics => vec![
                SettingsRow::GraphicsPreset,
                SettingsRow::ShadowQuality,
                SettingsRow::Clouds,
                SettingsRow::WaterQuality,
                SettingsRow::CloudCoverage,
                SettingsRow::Bloom,
                SettingsRow::Fxaa,
            ],
        }
    }

    /// Number of focusable entries on the active tab, including footer buttons.
    pub fn settings_focus_count(&self) -> usize {
        self.settings_rows().len() + SETTINGS_BUTTONS.len()
    }

    /// Row currently focused by the keyboard, if focus is not on a button.
    pub fn settings_focused_row(&self) -> Option<SettingsRow> {
        self.settings_rows().get(self.settings_selected_row).copied()
    }

    /// Footer button currently focused, if any.
    pub fn settings_focused_button(&self) -> Option<SettingsButton> {
        let row_count = self.settings_rows().len();
        SETTINGS_BUTTONS
            .get(self.settings_selected_row.checked_sub(row_count)?)
            .copied()
    }

    /// Moves keyboard focus up by one entry, wrapping through rows and buttons.
    pub fn settings_row_up(&mut self) {
        self.settings_move_focus(-1);
    }

    /// Moves keyboard focus down by one entry, wrapping through rows and buttons.
    pub fn settings_row_down(&mut self) {
        self.settings_move_focus(1);
    }

    fn settings_move_focus(&mut self, delta: i32) {
        let count = self.settings_focus_count();
        if count == 0 {
            return;
        }
        self.settings_selected_row = (self.settings_selected_row as i32 + delta).rem_euclid(count as i32) as usize;
        self.settings_hovered_row = None;
        self.settings_hovered_button = None;
    }

    fn settings_clear_hover(&mut self) {
        self.settings_hovered_tab = None;
        self.settings_hovered_row = None;
        self.settings_hovered_button = None;
    }

    /// Whether a row can currently be changed.
    pub fn settings_row_enabled(&self, row: SettingsRow) -> bool {
        match row {
            SettingsRow::GameMode if self.host_optional => false,
            SettingsRow::MeshWorkers => false,
            SettingsRow::TexturePack => {
                self.active_texture_pack.is_some()
                    || self
                        .settings
                        .texture_pack
                        .as_deref()
                        .is_some_and(|path| !path.is_empty())
            }
            _ => true,
        }
    }

    /// Whether `settings_adjust(delta)` would change `row`.
    ///
    /// Drawing dims a `<` or `>` arrow that would have no effect, and click
    /// handling uses the same answer so a click never appears to do nothing
    /// silently.
    pub fn settings_can_adjust(&self, row: SettingsRow, delta: i32) -> bool {
        if delta == 0 || !self.settings_row_enabled(row) {
            return false;
        }
        match row {
            SettingsRow::RenderDistance => {
                let value = self.settings.render_distance_chunks;
                if delta < 0 {
                    value > SETTINGS_MIN_RENDER_DISTANCE
                } else {
                    value < SETTINGS_MAX_RENDER_DISTANCE
                }
            }
            SettingsRow::Fov => {
                let value = if self.settings.fov_degrees.is_finite() {
                    self.settings.fov_degrees
                } else {
                    Settings::default().fov_degrees
                };
                if delta < 0 {
                    value > SETTINGS_MIN_FOV_DEGREES
                } else {
                    value < SETTINGS_MAX_FOV_DEGREES
                }
            }
            SettingsRow::MouseSensitivity => {
                let value = if self.settings.mouse_sensitivity.is_finite() {
                    self.settings.mouse_sensitivity
                } else {
                    Settings::default().mouse_sensitivity
                };
                if delta < 0 {
                    value > SETTINGS_MIN_MOUSE_SENSITIVITY
                } else {
                    value < SETTINGS_MAX_MOUSE_SENSITIVITY
                }
            }
            SettingsRow::MasterVolume => {
                volume_can_adjust(self.settings.master_volume, delta, Settings::default().master_volume)
            }
            SettingsRow::SfxVolume => {
                volume_can_adjust(self.settings.sfx_volume, delta, Settings::default().sfx_volume)
            }
            SettingsRow::MusicVolume => {
                volume_can_adjust(self.settings.music_volume, delta, Settings::default().music_volume)
            }
            SettingsRow::CloudCoverage => {
                let value = if self.settings.cloud_coverage.is_finite() {
                    self.settings.cloud_coverage.clamp(0.0, 1.0)
                } else {
                    Settings::default().cloud_coverage
                };
                if delta < 0 { value > 0.0 } else { value < 1.0 }
            }
            SettingsRow::GameMode
            | SettingsRow::TextureStyle
            | SettingsRow::UiScale
            | SettingsRow::GraphicsPreset
            | SettingsRow::ShadowQuality
            | SettingsRow::Clouds
            | SettingsRow::WaterQuality
            | SettingsRow::Bloom
            | SettingsRow::Fxaa => true,
            SettingsRow::ShowHud
            | SettingsRow::DebugOverlay
            | SettingsRow::Fullscreen
            | SettingsRow::TexturePack
            | SettingsRow::ResetUiFont
            | SettingsRow::ReloadUiFont
            | SettingsRow::AudioEnabled
            | SettingsRow::ReducedMotion
            | SettingsRow::Keybind(_)
            | SettingsRow::ResetKeybinds => true,
            SettingsRow::MeshWorkers | SettingsRow::UiFont => false,
        }
    }

    /// Left and right adjustment-arrow rectangles for every row.
    ///
    /// The vector is aligned with [`Self::settings_rows`]; rows that do not
    /// offer arrows (toggles, actions, read-only rows and disabled rows) are
    /// `None`. Drawing and hit-testing both use these rectangles.
    pub fn settings_arrow_rects(&self, screen: [f32; 2]) -> Vec<Option<(UiRect, UiRect)>> {
        self.settings_rows()
            .into_iter()
            .zip(self.settings_row_rects(screen))
            .map(|(row, rect)| {
                let shows_arrows = matches!(row.kind(), SettingsRowKind::Range | SettingsRowKind::Choice)
                    && self.settings_row_enabled(row);
                shows_arrows.then(|| settings_arrow_rects_for_metrics(rect, settings_metrics(screen)))
            })
            .collect()
    }

    /// Adjustment arrow under `cursor`: the row index and `-1`/`+1`.
    pub fn settings_arrow_at(&self, screen: [f32; 2], cursor: (f32, f32)) -> Option<(usize, i32)> {
        for (index, arrows) in self.settings_arrow_rects(screen).iter().enumerate() {
            if let Some((left, right)) = arrows {
                if left.contains(cursor.0, cursor.1) {
                    return Some((index, -1));
                }
                if right.contains(cursor.0, cursor.1) {
                    return Some((index, 1));
                }
            }
        }
        None
    }

    /// Display value drawn on the right of a row.
    pub fn settings_row_value(&self, row: SettingsRow, info: &SettingsInfo) -> String {
        match row {
            SettingsRow::GameMode => game_mode_label(self.settings.game_mode).to_string(),
            SettingsRow::ShowHud => checkbox(self.hud_visible),
            SettingsRow::DebugOverlay => checkbox(self.debug_overlay),
            SettingsRow::RenderDistance => format!(
                "{} chunks / {} blocks",
                self.settings.render_distance_chunks,
                self.settings.render_distance_chunks * 16
            ),
            SettingsRow::Fov => format!("{:.0} deg", self.settings.fov_degrees),
            SettingsRow::MouseSensitivity => format!("{:.2}x", self.settings.mouse_sensitivity),
            SettingsRow::Fullscreen => checkbox(self.settings.fullscreen),
            SettingsRow::UiScale => {
                crate::ui_scale::ui_scale_label(self.settings.ui_scale_percent, info.ui_scale_percent)
            }
            SettingsRow::MeshWorkers => format!("{} threads (launch)", info.mesh_workers.max(1)),
            SettingsRow::UiFont => {
                let name = if self.active_ui_font_name.is_empty() {
                    ob_render::default_ui_font_name()
                } else {
                    &self.active_ui_font_name
                };
                let source = if self.active_ui_font_source.is_empty() {
                    "built-in"
                } else {
                    &self.active_ui_font_source
                };
                shorten_row_value(&format!("{name} ({source})"))
            }
            SettingsRow::ResetUiFont => "Use built-in".to_string(),
            SettingsRow::ReloadUiFont => "Rescan fonts".to_string(),
            SettingsRow::TextureStyle => texture_style_label(self.settings.texture_style).to_string(),
            SettingsRow::TexturePack => {
                if let Some(name) = &self.active_texture_pack {
                    shorten_row_value(name)
                } else if self
                    .settings
                    .texture_pack
                    .as_deref()
                    .is_some_and(|path| !path.is_empty())
                {
                    "enable saved pack".to_string()
                } else {
                    "built-in only".to_string()
                }
            }
            SettingsRow::AudioEnabled => checkbox(self.settings.audio_enabled),
            SettingsRow::MasterVolume => volume_percent(self.settings.master_volume, Settings::default().master_volume),
            SettingsRow::SfxVolume => volume_percent(self.settings.sfx_volume, Settings::default().sfx_volume),
            SettingsRow::MusicVolume => volume_percent(self.settings.music_volume, Settings::default().music_volume),
            SettingsRow::Keybind(action) => self
                .settings
                .keybinds
                .binding(action)
                .map_or_else(|| "unbound".to_string(), |key| key.label().to_string()),
            SettingsRow::ResetKeybinds => "Defaults".to_string(),
            SettingsRow::ReducedMotion => checkbox(self.settings.reduced_motion),
            SettingsRow::GraphicsPreset => graphics_preset_label(self.settings.graphics_preset).to_string(),
            SettingsRow::ShadowQuality => shadow_quality_label(self.settings.shadow_quality).to_string(),
            SettingsRow::Clouds => cloud_quality_label(self.settings.clouds).to_string(),
            SettingsRow::WaterQuality => water_quality_label(self.settings.water_quality).to_string(),
            SettingsRow::CloudCoverage => {
                format!("{:.0}%", settings_fraction(self.settings.cloud_coverage) * 100.0)
            }
            SettingsRow::Bloom => checkbox(self.settings.bloom),
            SettingsRow::Fxaa => checkbox(self.settings.fxaa),
        }
    }

    /// One row ready to draw with its current value.
    pub fn settings_row_info(&self, row: SettingsRow, info: &SettingsInfo) -> SettingsRowInfo {
        SettingsRowInfo {
            row,
            label: row.label(),
            value: self.settings_row_value(row, info),
            kind: row.kind(),
            enabled: self.settings_row_enabled(row),
        }
    }

    /// All rows of the active tab, ready to draw.
    pub fn settings_row_infos(&self, info: &SettingsInfo) -> Vec<SettingsRowInfo> {
        self.settings_rows()
            .into_iter()
            .map(|row| self.settings_row_info(row, info))
            .collect()
    }

    /// Normalised fill for range rows, used by the slider-like display.
    pub fn settings_row_fraction(&self, row: SettingsRow) -> Option<f32> {
        match row {
            SettingsRow::RenderDistance => Some(range_fraction(
                self.settings.render_distance_chunks as f32,
                SETTINGS_MIN_RENDER_DISTANCE as f32,
                SETTINGS_MAX_RENDER_DISTANCE as f32,
            )),
            SettingsRow::Fov => Some(range_fraction(
                self.settings.fov_degrees,
                SETTINGS_MIN_FOV_DEGREES,
                SETTINGS_MAX_FOV_DEGREES,
            )),
            SettingsRow::MouseSensitivity => Some(range_fraction(
                self.settings.mouse_sensitivity,
                SETTINGS_MIN_MOUSE_SENSITIVITY,
                SETTINGS_MAX_MOUSE_SENSITIVITY,
            )),
            SettingsRow::MasterVolume => Some(range_fraction(
                settings_volume(self.settings.master_volume, Settings::default().master_volume),
                0.0,
                1.0,
            )),
            SettingsRow::SfxVolume => Some(range_fraction(
                settings_volume(self.settings.sfx_volume, Settings::default().sfx_volume),
                0.0,
                1.0,
            )),
            SettingsRow::MusicVolume => Some(range_fraction(
                settings_volume(self.settings.music_volume, Settings::default().music_volume),
                0.0,
                1.0,
            )),
            SettingsRow::CloudCoverage => Some(settings_fraction(self.settings.cloud_coverage)),
            _ => None,
        }
    }

    /// Changes the focused row by `delta` steps, clamping at range bounds.
    ///
    /// Returns the row when the caller must apply a side effect and persist the
    /// settings. Toggle rows flip once regardless of the magnitude; choice rows
    /// cycle in the direction of `delta`. The texture-pack row is an action: the
    /// client owns the active renderer source and performs the actual toggle.
    pub fn settings_adjust(&mut self, delta: i32) -> Option<SettingsRow> {
        if delta == 0 {
            return None;
        }
        let row = self.settings_focused_row()?;
        let changed = match row {
            SettingsRow::GameMode => {
                let index = GameMode::ALL
                    .iter()
                    .position(|mode| *mode == self.settings.game_mode)
                    .unwrap_or(0);
                let next = GameMode::ALL[step_index(index, GameMode::ALL.len(), delta)];
                if next == self.settings.game_mode {
                    false
                } else {
                    self.settings.game_mode = next;
                    true
                }
            }
            SettingsRow::ShowHud => {
                self.toggle_hud();
                true
            }
            SettingsRow::DebugOverlay => {
                self.toggle_debug_overlay();
                true
            }
            SettingsRow::RenderDistance => {
                let current = self
                    .settings
                    .render_distance_chunks
                    .clamp(SETTINGS_MIN_RENDER_DISTANCE, SETTINGS_MAX_RENDER_DISTANCE);
                let next = (current + delta.signum()).clamp(SETTINGS_MIN_RENDER_DISTANCE, SETTINGS_MAX_RENDER_DISTANCE);
                if next == self.settings.render_distance_chunks {
                    false
                } else {
                    self.settings.render_distance_chunks = next;
                    true
                }
            }
            SettingsRow::Fov => {
                let current = if self.settings.fov_degrees.is_finite() {
                    self.settings
                        .fov_degrees
                        .clamp(SETTINGS_MIN_FOV_DEGREES, SETTINGS_MAX_FOV_DEGREES)
                } else {
                    Settings::default().fov_degrees
                };
                let step = if delta > 0 {
                    SETTINGS_FOV_STEP
                } else {
                    -SETTINGS_FOV_STEP
                };
                let next = (current + step).clamp(SETTINGS_MIN_FOV_DEGREES, SETTINGS_MAX_FOV_DEGREES);
                if (next - self.settings.fov_degrees).abs() > f32::EPSILON {
                    self.settings.fov_degrees = next;
                    true
                } else {
                    false
                }
            }
            SettingsRow::MouseSensitivity => {
                let current = if self.settings.mouse_sensitivity.is_finite() {
                    self.settings
                        .mouse_sensitivity
                        .clamp(SETTINGS_MIN_MOUSE_SENSITIVITY, SETTINGS_MAX_MOUSE_SENSITIVITY)
                } else {
                    Settings::default().mouse_sensitivity
                };
                let step = if delta > 0 {
                    SETTINGS_SENSITIVITY_STEP
                } else {
                    -SETTINGS_SENSITIVITY_STEP
                };
                let next = (((current + step).clamp(SETTINGS_MIN_MOUSE_SENSITIVITY, SETTINGS_MAX_MOUSE_SENSITIVITY))
                    * 100.0)
                    .round()
                    / 100.0;
                if (next - self.settings.mouse_sensitivity).abs() > 0.0001 {
                    self.settings.mouse_sensitivity = next;
                    true
                } else {
                    false
                }
            }
            SettingsRow::Fullscreen => {
                self.settings.fullscreen = !self.settings.fullscreen;
                true
            }
            SettingsRow::UiScale => {
                let next =
                    crate::ui_scale::cycle_ui_scale(self.settings.ui_scale_percent, delta, self.ui_scale_percent);
                if next == self.settings.ui_scale_percent {
                    false
                } else {
                    self.settings.ui_scale_percent = next;
                    true
                }
            }
            SettingsRow::AudioEnabled => {
                self.settings.audio_enabled = !self.settings.audio_enabled;
                true
            }
            SettingsRow::MasterVolume => adjust_volume(
                &mut self.settings.master_volume,
                delta,
                Settings::default().master_volume,
            ),
            SettingsRow::SfxVolume => {
                adjust_volume(&mut self.settings.sfx_volume, delta, Settings::default().sfx_volume)
            }
            SettingsRow::MusicVolume => {
                adjust_volume(&mut self.settings.music_volume, delta, Settings::default().music_volume)
            }
            SettingsRow::ReducedMotion => {
                self.settings.reduced_motion = !self.settings.reduced_motion;
                true
            }
            SettingsRow::GraphicsPreset => {
                let current = GraphicsPreset::ALL
                    .iter()
                    .position(|preset| *preset == self.settings.graphics_preset)
                    .unwrap_or(2);
                let next = GraphicsPreset::ALL[step_index(current, GraphicsPreset::ALL.len(), delta)];
                if next == self.settings.graphics_preset {
                    false
                } else {
                    self.settings.graphics_preset = next;
                    self.settings.apply_graphics_preset();
                    true
                }
            }
            SettingsRow::ShadowQuality => {
                let next = cycle_quality(&ShadowQuality::ALL, self.settings.shadow_quality, delta);
                if next == self.settings.shadow_quality {
                    false
                } else {
                    self.settings.shadow_quality = next;
                    self.settings.mark_graphics_custom();
                    true
                }
            }
            SettingsRow::Clouds => {
                let next = cycle_quality(&CloudQuality::ALL, self.settings.clouds, delta);
                if next == self.settings.clouds {
                    false
                } else {
                    self.settings.clouds = next;
                    self.settings.mark_graphics_custom();
                    true
                }
            }
            SettingsRow::WaterQuality => {
                let next = cycle_quality(&WaterQuality::ALL, self.settings.water_quality, delta);
                if next == self.settings.water_quality {
                    false
                } else {
                    self.settings.water_quality = next;
                    self.settings.mark_graphics_custom();
                    true
                }
            }
            SettingsRow::CloudCoverage => {
                let current = settings_fraction(self.settings.cloud_coverage);
                let next = ((current + 0.05 * delta.signum() as f32).clamp(0.0, 1.0) * 100.0).round() / 100.0;
                if (next - current).abs() < 1e-4 {
                    false
                } else {
                    self.settings.cloud_coverage = next;
                    self.settings.mark_graphics_custom();
                    true
                }
            }
            SettingsRow::Bloom => {
                self.settings.bloom = !self.settings.bloom;
                self.settings.mark_graphics_custom();
                true
            }
            SettingsRow::Fxaa => {
                self.settings.fxaa = !self.settings.fxaa;
                self.settings.mark_graphics_custom();
                true
            }
            SettingsRow::MeshWorkers
            | SettingsRow::UiFont
            | SettingsRow::ResetUiFont
            | SettingsRow::ReloadUiFont
            | SettingsRow::Keybind(_)
            | SettingsRow::ResetKeybinds => false,
            SettingsRow::TextureStyle => {
                let index = TextureStyle::ALL
                    .iter()
                    .position(|style| *style == self.settings.texture_style)
                    .unwrap_or(0);
                let next = TextureStyle::ALL[step_index(index, TextureStyle::ALL.len(), delta)];
                if next == self.settings.texture_style {
                    false
                } else {
                    self.settings.texture_style = next;
                    true
                }
            }
            SettingsRow::TexturePack => self.settings_row_enabled(SettingsRow::TexturePack),
        };
        changed.then_some(row)
    }

    /// Activates the focused entry: footer buttons return their action, while
    /// rows behave like Right/Enter.
    pub fn settings_activate(&mut self) -> Option<SettingsAction> {
        if let Some(button) = self.settings_focused_button() {
            return match button {
                SettingsButton::Resume => {
                    self.resume();
                    Some(SettingsAction::Resume)
                }
                SettingsButton::SaveAndQuit => Some(SettingsAction::SaveAndQuit),
            };
        }
        let row = self.settings_focused_row()?;
        if !self.settings_row_enabled(row) || row.kind() == SettingsRowKind::ReadOnly {
            return None;
        }
        match row {
            SettingsRow::Keybind(action) => Some(SettingsAction::BeginBind(action)),
            SettingsRow::ResetKeybinds => Some(SettingsAction::ResetKeybinds),
            SettingsRow::ResetUiFont => Some(SettingsAction::ResetUiFont),
            SettingsRow::ReloadUiFont => Some(SettingsAction::ReloadUiFont),
            _ => self.settings_adjust(1).map(SettingsAction::Adjust),
        }
    }

    /// Starts capturing a key or mouse button for `action`.
    ///
    /// Refused while a text field owns the keyboard; the message explains why.
    pub fn begin_bind(&mut self, action: Action) -> bool {
        if self.text_input_active {
            self.binding_message = Some(KeybindError::TextInputActive.to_string());
            self.binding_message_error = true;
            return false;
        }
        self.bind_capture = Some(action);
        self.binding_message = Some(format!(
            "Press a key or mouse button for '{}' - Esc cancels.",
            action.label()
        ));
        self.binding_message_error = false;
        true
    }

    /// Cancels an in-progress capture and clears its feedback.
    pub fn cancel_bind(&mut self) {
        self.bind_capture = None;
        self.binding_message = None;
        self.binding_message_error = false;
    }

    /// Applies the captured key to the action waiting for it.
    ///
    /// Returns the rejection reason on a conflict, leaving capture active so the
    /// player can simply try another key.
    pub fn apply_bind(&mut self, key: BindKey) -> Result<(), KeybindError> {
        let Some(action) = self.bind_capture else {
            return Err(KeybindError::Invalid);
        };
        match self.settings.keybinds.rebind(action, key) {
            Ok(()) => {
                self.bind_capture = None;
                self.binding_message = Some(format!("Bound '{}' to {}.", action.label(), key.label()));
                self.binding_message_error = false;
                Ok(())
            }
            Err(error) => {
                self.binding_message = Some(error.to_string());
                self.binding_message_error = true;
                Err(error)
            }
        }
    }

    /// Clears the binding of the action currently capturing, or of the focused
    /// binding row when no capture is active.
    pub fn clear_binding(&mut self) -> Option<Action> {
        let action = self.bind_capture.or_else(|| match self.settings_focused_row() {
            Some(SettingsRow::Keybind(action)) => Some(action),
            _ => None,
        })?;
        if self.settings.keybinds.clear(action) {
            self.bind_capture = None;
            self.binding_message = Some(format!("Cleared the binding for '{}'.", action.label()));
            self.binding_message_error = false;
            Some(action)
        } else {
            self.binding_message = Some(format!("'{}' is already unbound.", action.label()));
            self.binding_message_error = false;
            None
        }
    }

    /// Restores every factory binding.
    pub fn reset_keybinds(&mut self) {
        self.settings.keybinds.reset_defaults();
        self.bind_capture = None;
        self.binding_message = Some("Controls reset to their default keys.".to_string());
        self.binding_message_error = false;
    }

    /// Action currently waiting for capture, if any.
    pub fn binding_action(&self) -> Option<Action> {
        self.bind_capture
    }

    /// Updates hover highlights and mouse row/button focus from [`Self::cursor`].
    pub fn settings_hover(&mut self, screen: [f32; 2]) {
        let panel = settings_panel_rect(screen);
        if !panel.contains(self.cursor.0, self.cursor.1) {
            self.settings_clear_hover();
            return;
        }
        let tabs = settings_tab_rects(screen);
        self.settings_hovered_tab = SettingsTab::ALL
            .iter()
            .zip(tabs.iter())
            .find(|(_, rect)| rect.contains(self.cursor.0, self.cursor.1))
            .map(|(tab, _)| *tab);
        let rows = self.settings_row_rects(screen);
        self.settings_hovered_row = rows.iter().position(|rect| rect.contains(self.cursor.0, self.cursor.1));
        let buttons = settings_button_rects(screen);
        self.settings_hovered_button = buttons
            .iter()
            .position(|rect| rect.contains(self.cursor.0, self.cursor.1));
        let row_count = self.settings_rows().len();
        if let Some(index) = self.settings_hovered_row {
            self.settings_selected_row = index;
        } else if let Some(index) = self.settings_hovered_button {
            self.settings_selected_row = row_count + index;
        }
    }

    /// Handles a click at [`Self::cursor`].
    ///
    /// Tab clicks switch tabs without closing the screen. Footer buttons act on
    /// click. On a row, the explicit `<`/`>` arrows change the value; clicking
    /// the row body only focuses it for range and choice rows, while toggles
    /// and actions still activate. Read-only rows do nothing.
    pub fn settings_click(&mut self, screen: [f32; 2]) -> Option<SettingsAction> {
        if self.screen != Screen::Settings {
            return None;
        }
        self.settings_hover(screen);
        if let Some(tab) = self.settings_hovered_tab {
            self.settings_select_tab(tab);
            return None;
        }
        let row_count = self.settings_rows().len();
        if let Some(index) = self.settings_hovered_button {
            self.settings_selected_row = row_count + index;
            return self.settings_activate();
        }
        let row_index = self.settings_hovered_row?;
        let row = *self.settings_rows().get(row_index)?;
        self.settings_selected_row = row_index;
        if let Some((arrow_row, delta)) = self.settings_arrow_at(screen, self.cursor)
            && arrow_row == row_index
        {
            return self.settings_adjust(delta).map(SettingsAction::Adjust);
        }
        match row.kind() {
            // Clicking the body only moves focus; the visible arrows are the
            // explicit adjustment affordance.
            SettingsRowKind::Range | SettingsRowKind::Choice | SettingsRowKind::ReadOnly => None,
            SettingsRowKind::Toggle | SettingsRowKind::Action => self.settings_activate(),
        }
    }

    /// Row hit rectangles for the active tab and target size.
    ///
    /// The Controls tab lays its 18 entries out in two columns so every action
    /// fits above the footer buttons; other tabs use the single-column list.
    pub fn settings_row_rects(&self, screen: [f32; 2]) -> Vec<UiRect> {
        let panel = settings_panel_rect(screen);
        let metrics = settings_metrics(screen);
        let buttons = settings_button_rects(screen);
        let start_y = panel.y + metrics.content_top;
        let rows = self.settings_rows();
        let count = rows.len();
        if count == 0 {
            return Vec::new();
        }
        if self.settings_tab == SettingsTab::Controls {
            // The Controls tab lays its entries out in two columns and keeps a
            // line free between the last hit box and the footer buttons for the
            // capture/conflict message.
            let per_column = count.div_ceil(2);
            let note_space = 16.0;
            let available = (buttons[0].y - note_space - start_y).max(1.0);
            let gap = 4.0;
            let height = (metrics
                .controls_row_height
                .min((available - gap * (per_column.saturating_sub(1)) as f32) / per_column as f32))
            .max(8.0);
            let step = if per_column > 1 {
                ((available - height) / (per_column - 1) as f32).min(metrics.controls_row_step)
            } else {
                0.0
            };
            let column_width = (panel.w - metrics.margin * 2.0 - metrics.controls_column_gap) / 2.0;
            return (0..count)
                .map(|index| {
                    let column = index / per_column;
                    let row = index % per_column;
                    UiRect::new(
                        panel.x + metrics.margin + column as f32 * (column_width + metrics.controls_column_gap),
                        start_y + row as f32 * step,
                        column_width,
                        height,
                    )
                })
                .collect();
        }
        // Fit every single-column row between the content top and the footer
        // buttons. At 100% the old 44 px pitch is kept; the extra Display rows
        // (GUI scale and UI font actions) compress the pitch slightly rather
        // than overlapping.
        let gap = 7.0;
        let available = (buttons[0].y - 6.0 - start_y).max(1.0);
        let height = metrics
            .row_height
            .min((available - gap * (count.saturating_sub(1)) as f32) / count as f32)
            .max(8.0);
        let step = if count > 1 {
            ((available - height) / (count - 1) as f32).min(metrics.row_step)
        } else {
            0.0
        };
        rows.iter()
            .enumerate()
            .map(|(index, _)| {
                UiRect::new(
                    panel.x + metrics.margin,
                    start_y + index as f32 * step,
                    panel.w - metrics.margin * 2.0,
                    height,
                )
            })
            .collect()
    }

    /// Opens the modal death screen and focuses the respawn button.
    pub fn open_death(&mut self) {
        self.screen = Screen::Death;
        self.death_selected = 0;
        self.death_hovered = None;
        self.death_cause = None;
        self.settings_clear_hover();
    }

    /// Shows the crosshair hit marker for a landed mob attack.
    pub fn flash_mob_hit(&mut self) {
        self.hit_marker_seconds = 0.22;
    }

    /// Leaves the death screen if it is open.
    pub fn clear_death(&mut self) {
        if self.screen == Screen::Death {
            self.screen = Screen::Playing;
        }
        self.death_cause = None;
    }

    /// Keeps keyboard focus on the death buttons without opening settings.
    pub fn focus_death_buttons(&mut self) {
        self.death_selected = self.death_selected.min(DEATH_ACTIONS.len() - 1);
        self.death_hovered = None;
    }

    /// Moves the death-screen selection, wrapping around both buttons.
    pub fn move_death_selection(&mut self, steps: i32) {
        let count = DEATH_ACTIONS.len() as i32;
        self.death_selected = (self.death_selected as i32 + steps).rem_euclid(count) as usize;
    }

    /// Death action currently focused, if any.
    pub fn selected_death_action(&self) -> Option<DeathAction> {
        DEATH_ACTIONS.get(self.death_selected).copied()
    }

    /// Runs the focused death-screen button.
    pub fn activate_death(&mut self) -> Option<DeathAction> {
        self.selected_death_action()
    }

    /// Updates the hovered death-screen button from the cursor.
    pub fn update_death_hover(&mut self, screen: [f32; 2]) {
        let rects = death_button_rects(screen);
        self.death_hovered = rects
            .iter()
            .position(|rect| rect.contains(self.cursor.0, self.cursor.1));
    }

    /// Handles a click on the death screen, returning the chosen action.
    pub fn click_death(&mut self, screen: [f32; 2]) -> Option<DeathAction> {
        self.update_death_hover(screen);
        let index = self.death_hovered?;
        self.death_selected = index;
        self.activate_death()
    }

    /// Shows a block name briefly near the crosshair after a completed break.
    pub fn flash_broken_block(&mut self, name: impl Into<String>) {
        self.break_flash = Some(name.into());
        self.break_flash_seconds = BLOCK_NAME_SECONDS;
    }

    /// Layout of the inventory grid for the current selection/scroll.
    pub fn inventory_layout(&self, screen: [f32; 2], item_count: usize) -> InventoryLayout {
        inventory_layout(screen, item_count, self.inventory.scroll)
    }

    /// Updates the hovered inventory slot from [`Self::cursor`].
    pub fn update_inventory_hover(&mut self, screen: [f32; 2], item_count: usize) {
        self.inventory.update_hover(self.cursor, screen, item_count);
    }

    /// Moves the inventory keyboard selection by grid columns/rows.
    pub fn move_inventory_selection(&mut self, screen: [f32; 2], item_count: usize, dx: i32, dy: i32) {
        self.inventory.move_selection(screen, item_count, dx, dy);
    }

    /// Scrolls the inventory grid by rows.
    pub fn scroll_inventory(&mut self, screen: [f32; 2], item_count: usize, rows: i32) {
        self.inventory.scroll_by(screen, item_count, rows);
    }

    /// Rects of the hotbar slots for a screen size.
    pub fn hotbar_rects(&self, screen: [f32; 2]) -> Vec<UiRect> {
        let total = HOTBAR_SLOTS as f32 * HOTBAR_SLOT_SIZE + (HOTBAR_SLOTS as f32 - 1.0) * HOTBAR_GAP;
        let start_x = (screen[0] - total) / 2.0;
        let y = screen[1] - HOTBAR_SLOT_SIZE - 18.0;
        (0..HOTBAR_SLOTS)
            .map(|slot| {
                let x = start_x + slot as f32 * (HOTBAR_SLOT_SIZE + HOTBAR_GAP);
                UiRect::new(x, y, HOTBAR_SLOT_SIZE, HOTBAR_SLOT_SIZE)
            })
            .collect()
    }
}

/// Footer button top, measured from the panel bottom.
const SETTINGS_BUTTON_Y_FROM_BOTTOM: f32 = 116.0;

/// Steps a cyclic index by one in the direction of `delta`.
fn step_index(index: usize, len: usize, delta: i32) -> usize {
    (index as i32 + delta.signum()).rem_euclid(len as i32) as usize
}

/// Title-cased gameplay-mode name.
pub fn game_mode_label(mode: GameMode) -> &'static str {
    match mode {
        GameMode::Survival => "Survival",
        GameMode::Creative => "Creative",
    }
}

/// Title-cased procedural texture-style name.
pub fn texture_style_label(style: TextureStyle) -> &'static str {
    match style {
        TextureStyle::Classic => "Classic",
        TextureStyle::Painterly => "Painterly",
        TextureStyle::Lush => "Lush",
        TextureStyle::Minimal => "Minimal",
    }
}

/// Title-cased graphics-preset name.
pub fn graphics_preset_label(preset: GraphicsPreset) -> &'static str {
    preset.label()
}

/// Title-cased shadow-quality name.
pub fn shadow_quality_label(quality: ShadowQuality) -> &'static str {
    quality.label()
}

/// Title-cased cloud-quality name.
pub fn cloud_quality_label(quality: CloudQuality) -> &'static str {
    quality.label()
}

/// Title-cased water-quality name.
pub fn water_quality_label(quality: WaterQuality) -> &'static str {
    quality.label()
}

/// Clamps cloud coverage to its supported range, substituting the default for
/// non-finite persisted values.
fn settings_fraction(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        Settings::default().cloud_coverage
    }
}

/// Cycles a small `Copy` enum-like list in the direction of `delta`.
fn cycle_quality<T: Copy + PartialEq>(all: &[T], current: T, delta: i32) -> T {
    let index = all.iter().position(|value| *value == current).unwrap_or(0);
    all[step_index(index, all.len(), delta)]
}

fn checkbox(value: bool) -> String {
    // The 8x8 bitmap font renders `[ ]` as two thin bars, which reads like
    // "| |" at UI scales. Words are unambiguous for screenshots and screen
    // readers alike.
    if value { "On" } else { "Off" }.to_string()
}

fn shorten(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut short: String = value.chars().take(max_chars.saturating_sub(2)).collect();
    short.push_str("..");
    short
}

fn shorten_row_value(value: &str) -> String {
    shorten(value, 26)
}

fn range_fraction(value: f32, min: f32, max: f32) -> f32 {
    if !value.is_finite() || max <= min {
        return 0.0;
    }
    ((value - min) / (max - min)).clamp(0.0, 1.0)
}

fn vital_value(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, VITAL_ICON_COUNT as f32 * VITAL_POINTS_PER_ICON)
    } else {
        0.0
    }
}

/// Responsive settings-screen metrics.
///
/// The release layouts are the 100% baseline. When the logical height drops
/// below 500 px (for example 960x540 at 150% GUI scale, which is 640x360
/// logical), a compact metric set keeps the tab strip, the ten Display rows and
/// the footer buttons inside the panel instead of clipping them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettingsMetrics {
    /// Whether the compact metric set is active.
    pub compact: bool,
    /// Horizontal inner margin.
    pub margin: f32,
    /// Y offset of the title.
    pub title_y: f32,
    /// Glyph scale of the title.
    pub title_scale: f32,
    /// Y offset of the version label.
    pub version_y: f32,
    /// Glyph scale of the version label.
    pub version_scale: f32,
    /// Y offset of the rule under the header.
    pub separator_y: f32,
    /// Y offset of the tab strip.
    pub tab_top: f32,
    /// Height of the tab strip.
    pub tab_height: f32,
    /// Gap between tabs.
    pub tab_gap: f32,
    /// Glyph scale of a tab label.
    pub tab_scale: f32,
    /// Y offset of the first row.
    pub content_top: f32,
    /// Preferred height of a row.
    pub row_height: f32,
    /// Preferred vertical pitch between rows.
    pub row_step: f32,
    /// Glyph scale of a row label.
    pub row_scale: f32,
    /// Width of one adjustment arrow.
    pub arrow_width: f32,
    /// Height of one adjustment arrow.
    pub arrow_height: f32,
    /// Gap between the two adjustment arrows.
    pub arrow_gap: f32,
    /// Right margin of the increment arrow.
    pub arrow_right_margin: f32,
    /// Gap between the value and the decrement arrow.
    pub value_gap: f32,
    /// Width of the range progress track.
    pub slider_width: f32,
    /// Gap between the slider track and the decrement arrow.
    pub slider_gap: f32,
    /// Glyph scale of a row value.
    pub value_scale: f32,
    /// Glyph scale of an arrow glyph.
    pub arrow_scale: f32,
    /// Width of a footer button.
    pub button_width: f32,
    /// Height of a footer button.
    pub button_height: f32,
    /// Gap between the footer buttons.
    pub button_gap: f32,
    /// Button top, measured from the panel bottom.
    pub button_y_from_bottom: f32,
    /// Glyph scale of a footer button label.
    pub button_scale: f32,
    /// Separator Y, measured from the panel bottom.
    pub footer_y_from_bottom: f32,
    /// First hint-line Y, measured from the panel bottom.
    pub hint_y_from_bottom: f32,
    /// Distance between hint lines.
    pub hint_step: f32,
    /// Glyph scale of a hint line.
    pub hint_scale: f32,
    /// Gap between the last row and a summary/source box.
    pub summary_gap: f32,
    /// Height of the summary box.
    pub summary_height: f32,
    /// Glyph scale of summary/source text.
    pub small_scale: f32,
    /// Height of one interactive Controls row.
    pub controls_row_height: f32,
    /// Preferred pitch of the Controls rows.
    pub controls_row_step: f32,
    /// Gap between the Controls columns.
    pub controls_column_gap: f32,
    /// Glyph scale of Controls key/action labels.
    pub controls_text_scale: f32,
}

/// Baseline (100%) settings metrics.
const SETTINGS_METRICS_BASE: SettingsMetrics = SettingsMetrics {
    compact: false,
    margin: SETTINGS_MARGIN,
    title_y: 18.0,
    title_scale: 2.6,
    version_y: 27.0,
    version_scale: 1.2,
    separator_y: 58.0,
    tab_top: SETTINGS_TAB_TOP,
    tab_height: SETTINGS_TAB_HEIGHT,
    tab_gap: SETTINGS_TAB_GAP,
    tab_scale: 1.45,
    content_top: SETTINGS_CONTENT_TOP,
    row_height: 30.0,
    row_step: SETTINGS_ROW_STEP,
    row_scale: 1.35,
    arrow_width: SETTINGS_ARROW_WIDTH,
    arrow_height: SETTINGS_ARROW_HEIGHT,
    arrow_gap: SETTINGS_ARROW_GAP,
    arrow_right_margin: SETTINGS_ARROW_RIGHT_MARGIN,
    value_gap: SETTINGS_VALUE_GAP,
    slider_width: SETTINGS_SLIDER_WIDTH,
    slider_gap: SETTINGS_SLIDER_GAP,
    value_scale: 1.25,
    arrow_scale: 1.4,
    button_width: SETTINGS_BUTTON_WIDTH,
    button_height: SETTINGS_BUTTON_HEIGHT,
    button_gap: SETTINGS_BUTTON_GAP,
    button_y_from_bottom: SETTINGS_BUTTON_Y_FROM_BOTTOM,
    button_scale: 1.5,
    footer_y_from_bottom: SETTINGS_FOOTER_Y_FROM_BOTTOM,
    hint_y_from_bottom: SETTINGS_HINT_Y_FROM_BOTTOM,
    hint_step: SETTINGS_HINT_STEP,
    hint_scale: 1.0,
    summary_gap: SETTINGS_SUMMARY_GAP,
    summary_height: SETTINGS_SUMMARY_HEIGHT,
    small_scale: 1.2,
    controls_row_height: SETTINGS_CONTROLS_ROW_HEIGHT,
    controls_row_step: SETTINGS_CONTROLS_ROW_STEP,
    controls_column_gap: SETTINGS_CONTROLS_COLUMN_GAP,
    controls_text_scale: 1.0,
};

/// Compact metrics used when the logical panel is shorter than 500 px.
const SETTINGS_METRICS_COMPACT: SettingsMetrics = SettingsMetrics {
    compact: true,
    margin: 16.0,
    title_y: 12.0,
    title_scale: 2.0,
    version_y: 16.0,
    version_scale: 1.0,
    separator_y: 40.0,
    tab_top: 46.0,
    tab_height: 26.0,
    tab_gap: 6.0,
    tab_scale: 1.15,
    content_top: 76.0,
    row_height: 20.0,
    row_step: 34.0,
    row_scale: 1.15,
    arrow_width: 20.0,
    arrow_height: 18.0,
    arrow_gap: 6.0,
    arrow_right_margin: 8.0,
    value_gap: 10.0,
    slider_width: 70.0,
    slider_gap: 10.0,
    value_scale: 1.1,
    arrow_scale: 1.15,
    button_width: 140.0,
    button_height: 28.0,
    button_gap: 16.0,
    button_y_from_bottom: 80.0,
    button_scale: 1.25,
    footer_y_from_bottom: 44.0,
    hint_y_from_bottom: 30.0,
    hint_step: 12.0,
    hint_scale: 0.95,
    summary_gap: 8.0,
    summary_height: 30.0,
    small_scale: 1.0,
    controls_row_height: 16.0,
    controls_row_step: 18.0,
    controls_column_gap: 8.0,
    controls_text_scale: 1.0,
};

/// Metrics for the current target: the 100% baseline or compact mode.
pub fn settings_metrics(screen: [f32; 2]) -> SettingsMetrics {
    if settings_panel_rect(screen).h < 420.0 {
        SETTINGS_METRICS_COMPACT
    } else {
        SETTINGS_METRICS_BASE
    }
}

/// Full bounds of the modal settings panel for a target size.
pub fn settings_panel_rect(screen: [f32; 2]) -> UiRect {
    let width = SETTINGS_PANEL_WIDTH.min((screen[0] - 24.0).max(1.0));
    let height = SETTINGS_PANEL_HEIGHT.min((screen[1] - 24.0).max(1.0));
    UiRect::new((screen[0] - width) / 2.0, (screen[1] - height) / 2.0, width, height)
}

/// Bounds of the settings tab buttons.
pub fn settings_tab_rects(screen: [f32; 2]) -> [UiRect; 6] {
    let panel = settings_panel_rect(screen);
    let metrics = settings_metrics(screen);
    let count = SettingsTab::ALL.len() as f32;
    let width = (panel.w - metrics.margin * 2.0 - metrics.tab_gap * (count - 1.0)) / count;
    std::array::from_fn(|index| {
        UiRect::new(
            panel.x + metrics.margin + index as f32 * (width + metrics.tab_gap),
            panel.y + metrics.tab_top,
            width,
            metrics.tab_height,
        )
    })
}

/// Left (`<`) and right (`>`) adjustment-arrow rectangles inside a row,
/// using the 100% metric set.
///
/// The returned rectangles sit at the right edge of the row, with the value
/// right-aligned before the decrement arrow so the two never overlap.
pub fn settings_arrow_rects_for(row: UiRect) -> (UiRect, UiRect) {
    settings_arrow_rects_for_metrics(row, SETTINGS_METRICS_BASE)
}

/// Left and right adjustment arrows for one row and metric set.
pub fn settings_arrow_rects_for_metrics(row: UiRect, metrics: SettingsMetrics) -> (UiRect, UiRect) {
    let arrow_height = metrics.arrow_height.min(row.h);
    let right = UiRect::new(
        row.x + row.w - metrics.arrow_right_margin - metrics.arrow_width,
        row.y + (row.h - arrow_height) / 2.0,
        metrics.arrow_width,
        arrow_height,
    );
    let left = UiRect::new(
        right.x - metrics.arrow_gap - metrics.arrow_width,
        right.y,
        metrics.arrow_width,
        arrow_height,
    );
    (left, right)
}

/// Bounds of the Resume and Save & quit buttons.
pub fn settings_button_rects(screen: [f32; 2]) -> [UiRect; 2] {
    let panel = settings_panel_rect(screen);
    let metrics = settings_metrics(screen);
    let total = metrics.button_width * 2.0 + metrics.button_gap;
    let x = panel.x + (panel.w - total) / 2.0;
    let y = panel.y + panel.h - metrics.button_y_from_bottom;
    [
        UiRect::new(x, y, metrics.button_width, metrics.button_height),
        UiRect::new(
            x + metrics.button_width + metrics.button_gap,
            y,
            metrics.button_width,
            metrics.button_height,
        ),
    ]
}

/// Draws the full modal settings screen over the dimmed world.
///
/// This is the public drawing entry point for the settings feature; later
/// client screens should add their own `draw_*` function alongside it instead
/// of growing this one.
pub fn draw_settings_screen(frame: &mut UiFrame, ui: &UiState, info: &SettingsInfo, screen: [f32; 2]) {
    let metrics = settings_metrics(screen);
    let [width, height] = screen;
    frame.rect(0.0, 0.0, width, height, [0.0, 0.0, 0.0, 0.55]);
    frame.rect(0.0, 0.0, width, height, [0.02, 0.03, 0.06, 0.18]);
    let panel = settings_panel_rect(screen);
    draw_settings_panel(frame, panel);
    draw_settings_header(frame, panel, metrics);
    draw_settings_tabs(frame, ui, screen, metrics);
    let rows = ui.settings_row_infos(info);
    let row_rects = ui.settings_row_rects(screen);
    let content_bottom = row_rects
        .last()
        .map_or(panel.y + metrics.content_top, |rect| rect.y + rect.h);
    match ui.settings_tab {
        SettingsTab::Game => {
            draw_settings_rows(frame, ui, &rows, &row_rects, screen);
            draw_game_summary(frame, ui, info, panel, content_bottom, metrics);
        }
        SettingsTab::Display => draw_settings_rows(frame, ui, &rows, &row_rects, screen),
        SettingsTab::Audio => {
            draw_settings_rows(frame, ui, &rows, &row_rects, screen);
            draw_audio_status(frame, info, panel, content_bottom, metrics);
        }
        SettingsTab::Controls => {
            draw_settings_rows(frame, ui, &rows, &row_rects, screen);
            draw_controls_help(frame, ui, panel, &row_rects, screen);
        }
        SettingsTab::Textures => {
            draw_settings_rows(frame, ui, &rows, &row_rects, screen);
            draw_texture_source(frame, info, panel, content_bottom, metrics);
        }
        SettingsTab::Graphics => draw_settings_rows(frame, ui, &rows, &row_rects, screen),
    }
    draw_settings_buttons(frame, ui, screen, metrics);
    draw_settings_footer(frame, panel, ui, metrics);
}

fn draw_settings_panel(frame: &mut UiFrame, panel: UiRect) {
    frame.rect(panel.x + 6.0, panel.y + 8.0, panel.w, panel.h, [0.0, 0.0, 0.0, 0.38]);
    frame.rect(panel.x, panel.y, panel.w, panel.h, [0.055, 0.068, 0.092, 0.985]);
    frame.outline(panel.x, panel.y, panel.w, panel.h, 2.0, [0.43, 0.49, 0.58, 1.0]);
    frame.outline(
        panel.x + 3.0,
        panel.y + 3.0,
        panel.w - 6.0,
        panel.h - 6.0,
        1.0,
        [0.15, 0.18, 0.24, 1.0],
    );
}

fn draw_settings_header(frame: &mut UiFrame, panel: UiRect, metrics: SettingsMetrics) {
    frame.title_text(
        panel.x + metrics.margin,
        panel.y + metrics.title_y,
        metrics.title_scale,
        [0.98, 0.99, 1.0, 1.0],
        "Settings",
    );
    let version = format!("OpenBlock v{}", ob_core::VERSION);
    let version_width = frame.text_width(metrics.version_scale, &version);
    frame.text(
        panel.x + panel.w - metrics.margin - version_width,
        panel.y + metrics.version_y,
        metrics.version_scale,
        [0.64, 0.70, 0.79, 1.0],
        &version,
    );
    frame.rect(
        panel.x + metrics.margin,
        panel.y + metrics.separator_y,
        panel.w - metrics.margin * 2.0,
        1.0,
        [0.24, 0.28, 0.35, 1.0],
    );
}

/// Scale that fits a settings tab label inside its tab.
///
/// Six tabs (the Graphics tab was added in v1.1) make the 620 px panel's tab
/// strip only ~84 px wide, while an 8-character label at the base 1.45 scale is
/// ~93 px. The scale is reduced per label instead of shrinking the whole strip,
/// and the 8 px inset keeps a visible margin between neighbours.
pub fn settings_tab_label_scale(rect: UiRect, label: &str, base_scale: f32) -> f32 {
    let glyphs = label.chars().count().max(1) as f32 * ob_render::GLYPH_SIZE;
    base_scale.min(((rect.w - 8.0) / glyphs).max(0.75))
}

fn draw_settings_tabs(frame: &mut UiFrame, ui: &UiState, screen: [f32; 2], metrics: SettingsMetrics) {
    let rects = settings_tab_rects(screen);
    for (index, tab) in SettingsTab::ALL.iter().enumerate() {
        let rect = rects[index];
        let scale = settings_tab_label_scale(rect, tab.name(), metrics.tab_scale);
        let label_y = rect.y + (rect.h - ob_render::line_height(scale)) / 2.0;
        let active = *tab == ui.settings_tab;
        let hovered = ui.settings_hovered_tab == Some(*tab);
        let background = if active {
            [0.13, 0.18, 0.26, 1.0]
        } else if hovered {
            [0.10, 0.13, 0.18, 0.95]
        } else {
            [0.07, 0.09, 0.12, 0.65]
        };
        frame.rect(rect.x, rect.y, rect.w, rect.h, background);
        if active {
            frame.rect(rect.x, rect.y + rect.h - 3.0, rect.w, 3.0, [0.96, 0.76, 0.32, 1.0]);
        } else if hovered {
            frame.outline(rect.x, rect.y, rect.w, rect.h, 1.0, [0.40, 0.46, 0.55, 1.0]);
        }
        let color = if active {
            [1.0, 0.98, 0.92, 1.0]
        } else if hovered {
            [0.85, 0.90, 0.96, 1.0]
        } else {
            [0.62, 0.68, 0.76, 1.0]
        };
        frame.text_centered(rect.center().0, label_y, scale, color, tab.name());
    }
}

fn draw_settings_rows(frame: &mut UiFrame, ui: &UiState, rows: &[SettingsRowInfo], rects: &[UiRect], screen: [f32; 2]) {
    let metrics = settings_metrics(screen);
    let arrows = ui.settings_arrow_rects(screen);
    let hovered_arrow = ui.settings_arrow_at(screen, ui.cursor);
    let inset = if metrics.compact { 10.0 } else { 16.0 };
    for (index, (info, rect)) in rows.iter().zip(rects.iter()).enumerate() {
        if info.row == SettingsRow::MeshWorkers {
            frame.rect(rect.x, rect.y - 9.0, rect.w, 1.0, [0.19, 0.23, 0.29, 0.9]);
        }
        let focused = index == ui.settings_selected_row;
        let hovered = ui.settings_hovered_row == Some(index);
        if focused || hovered {
            frame.rect(rect.x, rect.y, rect.w, rect.h, [0.16, 0.21, 0.29, 0.96]);
            frame.outline(
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                1.0,
                if focused {
                    [0.95, 0.79, 0.35, 0.95]
                } else {
                    [0.42, 0.49, 0.59, 0.85]
                },
            );
            frame.rect(
                rect.x,
                rect.y + 4.0,
                3.0,
                (rect.h - 8.0).max(1.0),
                [0.96, 0.76, 0.32, 1.0],
            );
        }
        let label_color = if info.enabled {
            [0.92, 0.95, 0.99, 1.0]
        } else {
            [0.50, 0.55, 0.63, 1.0]
        };
        let label_y = rect.y + (rect.h - ob_render::line_height(metrics.row_scale)) / 2.0;
        frame.text(rect.x + inset, label_y, metrics.row_scale, label_color, info.label);
        let row_arrows = arrows.get(index).copied().flatten();
        let value_right = row_arrows.map_or(rect.x + rect.w - inset, |(left, _)| left.x - metrics.value_gap);
        let value_color = settings_value_color(info);
        let value_width = frame.text_width(metrics.value_scale, &info.value);
        let value_x = (value_right - value_width).max(rect.x + inset);
        frame.text(value_x, label_y, metrics.value_scale, value_color, &info.value);
        if let Some((left, right)) = row_arrows {
            let left_active = ui.settings_can_adjust(info.row, -1);
            let right_active = ui.settings_can_adjust(info.row, 1);
            draw_settings_arrow(
                frame,
                left,
                "<",
                left_active,
                hovered_arrow == Some((index, -1)),
                metrics,
            );
            draw_settings_arrow(
                frame,
                right,
                ">",
                right_active,
                hovered_arrow == Some((index, 1)),
                metrics,
            );
        }
        if let Some(fraction) = ui.settings_row_fraction(info.row) {
            let track_width = metrics.slider_width.min((rect.w * 0.28).max(1.0));
            let track_x = row_arrows.map_or(rect.x + rect.w - track_width - inset, |(left, _)| {
                left.x - metrics.slider_gap - track_width
            });
            let track_y = rect.y + rect.h - 7.0;
            frame.rect(track_x, track_y, track_width, 3.0, [0.10, 0.12, 0.16, 1.0]);
            let filled = track_width * fraction;
            if filled > 0.0 {
                frame.rect(track_x, track_y, filled, 3.0, [0.88, 0.69, 0.28, 1.0]);
            }
            frame.rect(track_x + filled - 2.0, track_y - 2.0, 4.0, 7.0, [0.98, 0.83, 0.43, 1.0]);
        }
    }
}

fn draw_settings_arrow(
    frame: &mut UiFrame,
    rect: UiRect,
    glyph: &str,
    enabled: bool,
    hovered: bool,
    metrics: SettingsMetrics,
) {
    let background = if hovered && enabled {
        [0.28, 0.34, 0.44, 0.98]
    } else if enabled {
        [0.13, 0.17, 0.23, 0.92]
    } else {
        [0.09, 0.11, 0.14, 0.75]
    };
    frame.rect(rect.x, rect.y, rect.w, rect.h, background);
    let border = if hovered && enabled {
        [0.98, 0.84, 0.46, 1.0]
    } else if enabled {
        [0.42, 0.49, 0.59, 0.95]
    } else {
        [0.22, 0.25, 0.30, 0.8]
    };
    frame.outline(rect.x, rect.y, rect.w, rect.h, 1.0, border);
    let color = if enabled {
        [0.95, 0.97, 1.0, 1.0]
    } else {
        [0.45, 0.50, 0.57, 0.9]
    };
    let text_height = ob_render::line_height(metrics.arrow_scale);
    frame.text_centered(
        rect.center().0,
        rect.y + (rect.h - text_height) / 2.0,
        metrics.arrow_scale,
        color,
        glyph,
    );
}

fn settings_value_color(info: &SettingsRowInfo) -> [f32; 4] {
    if !info.enabled {
        return [0.48, 0.53, 0.61, 1.0];
    }
    match info.kind {
        SettingsRowKind::Toggle => {
            if info.value == "On" {
                [0.72, 0.93, 0.55, 1.0]
            } else {
                [0.55, 0.60, 0.68, 1.0]
            }
        }
        SettingsRowKind::Choice => [0.98, 0.83, 0.44, 1.0],
        SettingsRowKind::Range | SettingsRowKind::Action => [0.86, 0.91, 1.0, 1.0],
        SettingsRowKind::ReadOnly => [0.55, 0.60, 0.68, 1.0],
    }
}

fn draw_game_summary(
    frame: &mut UiFrame,
    ui: &UiState,
    info: &SettingsInfo,
    panel: UiRect,
    content_bottom: f32,
    metrics: SettingsMetrics,
) {
    let x = panel.x + metrics.margin;
    let y = content_bottom + metrics.summary_gap;
    let width = panel.w - metrics.margin * 2.0;
    frame.rect(x, y, width, metrics.summary_height, [0.09, 0.11, 0.15, 0.96]);
    frame.outline(x, y, width, metrics.summary_height, 1.0, [0.20, 0.24, 0.31, 1.0]);
    let mode = game_mode_label(ui.settings.game_mode);
    let health = vital_value(info.health);
    let hunger = vital_value(info.hunger);
    let text = format!("{mode}   Health {health:.0} / 20   Hunger {hunger:.0} / 20");
    frame.text(
        x + 14.0,
        y + (metrics.summary_height - ob_render::line_height(metrics.small_scale)) / 2.0,
        metrics.small_scale,
        [0.90, 0.94, 0.99, 1.0],
        &text,
    );
    if ui.host_optional {
        frame.text(
            x,
            y + metrics.summary_height + 8.0,
            0.95,
            [0.96, 0.80, 0.42, 1.0],
            "No world loaded: game mode is chosen when you start a world.",
        );
    }
}

fn draw_texture_source(
    frame: &mut UiFrame,
    info: &SettingsInfo,
    panel: UiRect,
    content_bottom: f32,
    metrics: SettingsMetrics,
) {
    let x = panel.x + metrics.margin;
    let y = content_bottom + metrics.summary_gap;
    let source = if info.texture_source.is_empty() {
        "built-in"
    } else {
        &info.texture_source
    };
    let resolution = if info.texture_resolution == 0 {
        "resolution unknown".to_string()
    } else {
        format!("{} px", info.texture_resolution)
    };
    let source = shorten(source, 20);
    let text = format!("Active texture source: {source} ({resolution})");
    frame.rect(
        x,
        y - 10.0,
        panel.w - metrics.margin * 2.0,
        1.0,
        [0.19, 0.23, 0.29, 0.9],
    );
    frame.text(x, y, metrics.small_scale, [0.80, 0.86, 0.94, 1.0], &text);
}

fn draw_audio_status(
    frame: &mut UiFrame,
    info: &SettingsInfo,
    panel: UiRect,
    content_bottom: f32,
    metrics: SettingsMetrics,
) {
    let x = panel.x + metrics.margin;
    let y = content_bottom + metrics.summary_gap;
    let status = if info.audio_status.is_empty() {
        "off"
    } else {
        info.audio_status.as_str()
    };
    frame.rect(
        x,
        y - 10.0,
        panel.w - metrics.margin * 2.0,
        1.0,
        [0.19, 0.23, 0.29, 0.9],
    );
    frame.text(
        x,
        y,
        metrics.small_scale,
        [0.80, 0.86, 0.94, 1.0],
        &format!("Output: {}", shorten(status, 66)),
    );
    frame.text(
        x,
        y + 18.0,
        0.95,
        [0.62, 0.68, 0.76, 1.0],
        "Volumes apply immediately; 5% steps with Left/Right or the wheel.",
    );
}

fn draw_controls_help(frame: &mut UiFrame, ui: &UiState, panel: UiRect, rects: &[UiRect], screen: [f32; 2]) {
    let metrics = settings_metrics(screen);
    let buttons = settings_button_rects(screen);
    let top = (rects
        .last()
        .map_or(panel.y + metrics.content_top, |rect| rect.y + rect.h)
        + 6.0)
        .min(buttons[0].y - ob_render::line_height(0.95) - 4.0);
    let x = panel.x + metrics.margin;
    // Only one note line fits between the last keybind row and the footer
    // buttons at small logical sizes. The message names the conflict or the
    // action waiting for capture, so no information is lost.
    let conflicts = ui.settings.keybinds.conflict_lines();
    let (message, color) = if let Some(action) = ui.binding_action() {
        (
            format!("Press a key or mouse button for '{}' - Esc cancels.", action.label()),
            [0.98, 0.83, 0.44, 1.0],
        )
    } else if let Some(message) = &ui.binding_message {
        (
            message.clone(),
            if ui.binding_message_error {
                [1.0, 0.66, 0.56, 1.0]
            } else {
                [0.72, 0.93, 0.55, 1.0]
            },
        )
    } else if let Some(conflict) = conflicts.first() {
        (conflict.clone(), [1.0, 0.66, 0.56, 1.0])
    } else {
        (
            "Bindings apply immediately and save automatically.".to_string(),
            [0.62, 0.68, 0.76, 1.0],
        )
    };
    frame.rect(
        x,
        top - 6.0,
        panel.w - metrics.margin * 2.0,
        1.0,
        [0.19, 0.23, 0.29, 0.9],
    );
    let max_chars = (((panel.w - metrics.margin * 2.0) / ob_render::glyph_advance(0.95)).max(24.0)) as usize;
    frame.text(x, top, 0.95, color, &shorten(&message, max_chars));
}

fn draw_settings_buttons(frame: &mut UiFrame, ui: &UiState, screen: [f32; 2], metrics: SettingsMetrics) {
    let rects = settings_button_rects(screen);
    let row_count = ui.settings_rows().len();
    for (index, button) in SETTINGS_BUTTONS.iter().enumerate() {
        let rect = rects[index];
        let focused = ui.settings_selected_row == row_count + index;
        let hovered = ui.settings_hovered_button == Some(index);
        let active = focused || hovered;
        let (background, border) = match button {
            SettingsButton::Resume => {
                if active {
                    ([0.17, 0.27, 0.20, 0.98], [0.72, 0.94, 0.46, 1.0])
                } else {
                    ([0.10, 0.15, 0.13, 0.98], [0.29, 0.41, 0.32, 1.0])
                }
            }
            SettingsButton::SaveAndQuit => {
                if active {
                    ([0.30, 0.10, 0.10, 0.98], [0.98, 0.55, 0.45, 1.0])
                } else {
                    ([0.14, 0.08, 0.09, 0.98], [0.41, 0.21, 0.23, 1.0])
                }
            }
        };
        frame.rect(rect.x, rect.y, rect.w, rect.h, background);
        frame.outline(rect.x, rect.y, rect.w, rect.h, if active { 2.0 } else { 1.0 }, border);
        let label_color = if active {
            [1.0, 1.0, 1.0, 1.0]
        } else {
            [0.84, 0.88, 0.93, 1.0]
        };
        let text_height = ob_render::line_height(metrics.button_scale);
        frame.text_centered(
            rect.center().0,
            rect.y + (rect.h - text_height) / 2.0,
            metrics.button_scale,
            label_color,
            button.label(),
        );
    }
}

fn draw_settings_footer(frame: &mut UiFrame, panel: UiRect, ui: &UiState, metrics: SettingsMetrics) {
    let separator_y = panel.y + panel.h - metrics.footer_y_from_bottom;
    frame.rect(
        panel.x + metrics.margin,
        separator_y,
        panel.w - metrics.margin * 2.0,
        1.0,
        [0.24, 0.28, 0.35, 1.0],
    );
    let hint_y = panel.y + panel.h - metrics.hint_y_from_bottom;
    // Contextual help: the GUI-scale row is the one setting whose visual
    // effect is not obvious from its label alone.
    let ui_scale_help = (ui.settings_tab == SettingsTab::Display
        && ui.settings_focused_row() == Some(SettingsRow::UiScale))
    .then_some([
        "GUI scale applies immediately; Auto grows with the window.",
        "Panels reflow to stay on screen. Esc: back",
    ]);
    let ui_font_help = (ui.settings_tab == SettingsTab::Display
        && matches!(
            ui.settings_focused_row(),
            Some(SettingsRow::UiFont | SettingsRow::ResetUiFont | SettingsRow::ReloadUiFont)
        ))
    .then_some([
        "Place Minecraftia.ttf in <data>/fonts/ or use --ui-font PATH.",
        "Reset uses built-in Monocraft; Reload re-scans data/system fonts.",
    ]);
    let lines: &[&str] = if let Some(lines) = ui_scale_help.as_ref() {
        lines
    } else if let Some(lines) = ui_font_help.as_ref() {
        lines
    } else if ui.settings_tab == SettingsTab::Controls {
        &SETTINGS_CONTROLS_HINT_LINES
    } else {
        &SETTINGS_HINT_LINES
    };
    for (index, line) in lines.iter().enumerate() {
        let max_chars =
            (((panel.w - metrics.margin * 2.0) / ob_render::glyph_advance(metrics.hint_scale)).max(24.0)) as usize;
        frame.text_centered(
            panel.x + panel.w / 2.0,
            hint_y + index as f32 * metrics.hint_step,
            metrics.hint_scale,
            [0.62, 0.68, 0.76, 1.0],
            &shorten(line, max_chars),
        );
    }
}

/// Fill state of every heart icon for a health value in `0..=20`.
///
/// Each icon represents two health points: an odd remaining point leaves the
/// next heart half-full.
pub fn heart_states(health: f32) -> [HeartState; VITAL_ICON_COUNT] {
    let health = if health.is_finite() {
        health.clamp(0.0, VITAL_ICON_COUNT as f32 * VITAL_POINTS_PER_ICON)
    } else {
        0.0
    };
    std::array::from_fn(
        |index| match vital_fill(health - index as f32 * VITAL_POINTS_PER_ICON) {
            VitalFill::Full => HeartState::Full,
            VitalFill::Half => HeartState::Half,
            VitalFill::Empty => HeartState::Empty,
        },
    )
}

/// Fill state of every hunger icon for a hunger value in `0..=20`.
pub fn hunger_states(hunger: f32) -> [HungerState; VITAL_ICON_COUNT] {
    let hunger = if hunger.is_finite() {
        hunger.clamp(0.0, VITAL_ICON_COUNT as f32 * VITAL_POINTS_PER_ICON)
    } else {
        0.0
    };
    std::array::from_fn(
        |index| match vital_fill(hunger - index as f32 * VITAL_POINTS_PER_ICON) {
            VitalFill::Full => HungerState::Full,
            VitalFill::Half => HungerState::Half,
            VitalFill::Empty => HungerState::Empty,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VitalFill {
    Full,
    Half,
    Empty,
}

fn vital_fill(points: f32) -> VitalFill {
    if points >= VITAL_POINTS_PER_ICON {
        VitalFill::Full
    } else if points >= 1.0 {
        VitalFill::Half
    } else {
        VitalFill::Empty
    }
}

/// Positions of the ten heart and ten hunger icons above the hotbar.
pub fn vitals_layout(screen: [f32; 2]) -> (Vec<UiRect>, Vec<UiRect>) {
    let hotbar_top = screen[1] - HOTBAR_SLOT_SIZE - 18.0;
    let row = |y: f32| {
        (0..VITAL_ICON_COUNT)
            .map(|index| {
                UiRect::new(
                    VITAL_MARGIN_X + index as f32 * (VITAL_ICON_SIZE + VITAL_ICON_GAP),
                    y,
                    VITAL_ICON_SIZE,
                    VITAL_ICON_SIZE,
                )
            })
            .collect::<Vec<_>>()
    };
    let hunger_y = hotbar_top - VITAL_ICON_SIZE - 6.0;
    let hearts_y = hunger_y - VITAL_ICON_SIZE - 4.0;
    (row(hearts_y), row(hunger_y))
}

/// Draws the survival health and hunger rows from primitive rectangles.
pub fn draw_vitals(frame: &mut UiFrame, screen: [f32; 2], health: f32, hunger: f32) {
    let (hearts, hunger_icons) = vitals_layout(screen);
    for (rect, state) in hearts.iter().zip(heart_states(health)) {
        draw_heart(frame, *rect, state);
    }
    for (rect, state) in hunger_icons.iter().zip(hunger_states(hunger)) {
        draw_hunger(frame, *rect, state);
    }
}

/// Positions of the ten armor pips to the right of the hunger row.
pub fn armor_layout(screen: [f32; 2]) -> Vec<UiRect> {
    let (_, hunger) = vitals_layout(screen);
    let Some(first_hunger) = hunger.first() else {
        return Vec::new();
    };
    let x = first_hunger.x + VITAL_ICON_COUNT as f32 * (VITAL_ICON_SIZE + VITAL_ICON_GAP) + ARMOR_ROW_GAP;
    (0..ARMOR_ICON_COUNT)
        .map(|index| {
            UiRect::new(
                x + index as f32 * (VITAL_ICON_SIZE + VITAL_ICON_GAP),
                first_hunger.y,
                VITAL_ICON_SIZE,
                VITAL_ICON_SIZE,
            )
        })
        .collect()
}

/// Pixel-art shield shape: broad shoulders tapering to a point.
fn shield_shape(rect: UiRect) -> [UiRect; 5] {
    let u = rect.w / 8.0;
    let v = rect.h / 8.0;
    [
        UiRect::new(rect.x + u, rect.y, 6.0 * u, v),
        UiRect::new(rect.x, rect.y + v, 8.0 * u, 3.0 * v),
        UiRect::new(rect.x + u, rect.y + 4.0 * v, 6.0 * u, 2.0 * v),
        UiRect::new(rect.x + 2.0 * u, rect.y + 6.0 * v, 4.0 * u, v),
        UiRect::new(rect.x + 3.0 * u, rect.y + 7.0 * v, 2.0 * u, v),
    ]
}

/// Draws the survival armor points next to the hunger row.
///
/// Ten pips represent up to 20 defence points (two per pip), so odd defence
/// totals show a half shield. The numeric value follows the row. A zero-defence
/// player still sees the dim empty row so the HUD layout never jumps when a
/// chestplate is equipped.
pub fn draw_armor(frame: &mut UiFrame, screen: [f32; 2], defense: u8) {
    let rects = armor_layout(screen);
    let Some(first) = rects.first() else {
        return;
    };
    let points = f32::from(defense);
    for (index, rect) in rects.iter().enumerate() {
        let fill = (points - index as f32 * ARMOR_POINTS_PER_ICON) / ARMOR_POINTS_PER_ICON;
        let state = if fill >= 1.0 {
            VitalFill::Full
        } else if fill >= 0.5 {
            VitalFill::Half
        } else {
            VitalFill::Empty
        };
        let shape = shield_shape(*rect);
        for part in shape {
            frame.rect(part.x, part.y, part.w, part.h, [0.10, 0.12, 0.16, 0.92]);
        }
        let steel = [0.70, 0.78, 0.88, 1.0];
        let half_x = rect.x + rect.w / 2.0;
        for part in shape {
            match state {
                VitalFill::Full => frame.rect(part.x, part.y, part.w, part.h, steel),
                VitalFill::Half => {
                    let right = (part.x + part.w).min(half_x);
                    if right > part.x {
                        frame.rect(part.x, part.y, right - part.x, part.h, steel);
                    }
                }
                VitalFill::Empty => {}
            }
        }
    }
    let label = format!("{defense} DEF");
    let label_x = first.x + ARMOR_ICON_COUNT as f32 * (VITAL_ICON_SIZE + VITAL_ICON_GAP) + 6.0;
    frame.text(label_x, first.y + 3.0, 1.2, [0.82, 0.88, 0.96, 1.0], &label);
}

/// Pixel-art heart shape: two top bumps, a wide body and a tapering tip.
fn heart_shape(rect: UiRect) -> [UiRect; 5] {
    let u = rect.w / 7.0;
    let v = rect.h / 6.0;
    [
        UiRect::new(rect.x + u, rect.y, 2.0 * u, 2.0 * v),
        UiRect::new(rect.x + 4.0 * u, rect.y, 2.0 * u, 2.0 * v),
        UiRect::new(rect.x, rect.y + 2.0 * v, 7.0 * u, 2.0 * v),
        UiRect::new(rect.x + u, rect.y + 4.0 * v, 5.0 * u, v),
        UiRect::new(rect.x + 3.0 * u, rect.y + 5.0 * v, u, v),
    ]
}

fn draw_heart(frame: &mut UiFrame, rect: UiRect, state: HeartState) {
    let shape = heart_shape(rect);
    let empty = [0.12, 0.04, 0.05, 0.92];
    for part in shape {
        frame.rect(part.x, part.y, part.w, part.h, empty);
    }
    let full = [0.86, 0.15, 0.20, 1.0];
    let half_x = rect.x + rect.w / 2.0;
    for part in shape {
        match state {
            HeartState::Full => frame.rect(part.x, part.y, part.w, part.h, full),
            HeartState::Half => {
                let right = (part.x + part.w).min(half_x);
                if right > part.x {
                    frame.rect(part.x, part.y, right - part.x, part.h, full);
                }
            }
            HeartState::Empty => {}
        }
    }
}

/// Pixel-art drumstick shape: a meat blob with a small bone at the lower right.
fn drumstick_shape(rect: UiRect) -> [UiRect; 5] {
    let u = rect.w / 7.0;
    let v = rect.h / 6.0;
    [
        UiRect::new(rect.x + 2.0 * u, rect.y + v, 4.0 * u, 2.0 * v),
        UiRect::new(rect.x + u, rect.y + 2.0 * v, 5.0 * u, 2.0 * v),
        UiRect::new(rect.x + 2.0 * u, rect.y + 4.0 * v, 3.0 * u, v),
        UiRect::new(rect.x + 5.0 * u, rect.y + 4.0 * v, 2.0 * u, v),
        UiRect::new(rect.x + 6.0 * u, rect.y + 5.0 * v, u, v),
    ]
}

fn draw_hunger(frame: &mut UiFrame, rect: UiRect, state: HungerState) {
    let shape = drumstick_shape(rect);
    let dark_meat = [0.16, 0.09, 0.05, 0.92];
    let dark_bone = [0.24, 0.21, 0.16, 0.88];
    for (index, part) in shape.iter().enumerate() {
        let color = if index >= 3 { dark_bone } else { dark_meat };
        frame.rect(part.x, part.y, part.w, part.h, color);
    }
    let meat = [0.82, 0.51, 0.24, 1.0];
    let bone = [0.93, 0.88, 0.73, 1.0];
    let half_x = rect.x + rect.w / 2.0;
    for (index, part) in shape.iter().enumerate() {
        let color = if index >= 3 { bone } else { meat };
        match state {
            HungerState::Full => frame.rect(part.x, part.y, part.w, part.h, color),
            HungerState::Half => {
                let right = (part.x + part.w).min(half_x);
                if right > part.x {
                    frame.rect(part.x, part.y, right - part.x, part.h, color);
                }
            }
            HungerState::Empty => {}
        }
    }
}

/// Draws the red damage vignette with four inward-fading bands.
pub fn draw_damage_flash(frame: &mut UiFrame, screen: [f32; 2], alpha: f32) {
    if !alpha.is_finite() || alpha <= 0.0 {
        return;
    }
    let alpha = alpha.clamp(0.0, 1.0);
    let band = 10.0;
    for layer in 0..4 {
        let inset = layer as f32 * band;
        let layer_alpha = alpha * (0.26 - layer as f32 * 0.05).max(0.0);
        let color = [0.86, 0.04, 0.06, layer_alpha];
        frame.rect(0.0, inset, screen[0], band, color);
        frame.rect(0.0, screen[1] - inset - band, screen[0], band, color);
        frame.rect(inset, 0.0, band, screen[1], color);
        frame.rect(screen[0] - inset - band, 0.0, band, screen[1], color);
    }
}

/// Draws the pickup/hint toasts above the vitals, newest closest to them.
pub fn draw_toasts(frame: &mut UiFrame, toasts: &ToastQueue, screen: [f32; 2]) {
    let (hearts, _) = vitals_layout(screen);
    let Some(first) = hearts.first() else {
        return;
    };
    let scale = 1.2;
    let line_step = ob_render::line_height(scale) + 4.0;
    for (index, toast) in toasts.iter().rev().enumerate() {
        let fade = (toast.remaining / 0.5).clamp(0.0, 1.0);
        let alpha = fade * 0.95;
        let y = first.y - 22.0 - index as f32 * line_step;
        let max_chars =
            (((screen[0] - VITAL_MARGIN_X * 2.0 - 12.0) / ob_render::glyph_advance(scale)).max(8.0)) as usize;
        let text = shorten(&toast.text, max_chars);
        let width = frame.text_width(scale, &text);
        frame.rect(
            VITAL_MARGIN_X,
            y - 2.0,
            width + 8.0,
            ob_render::line_height(scale) + 4.0,
            [0.02, 0.03, 0.05, alpha * 0.65],
        );
        let color = match toast.kind {
            ToastKind::Info => [0.95, 0.97, 1.0, alpha],
            ToastKind::Success => [0.62, 0.94, 0.56, alpha],
            ToastKind::Warning => [1.0, 0.62, 0.55, alpha],
        };
        frame.text(VITAL_MARGIN_X + 4.0, y, scale, color, &text);
    }
}

/// Positions of the two death-screen buttons.
///
/// Below 500 logical pixels the buttons use a compact 34 px height and sit
/// higher so the "You died" block, both buttons and the key hint stay clear of
/// the vitals row (e.g. 960x540 at 150% GUI scale is 640x360 logical).
pub fn death_button_rects(screen: [f32; 2]) -> [UiRect; 2] {
    if screen[1] < 500.0 {
        let width = DEATH_BUTTON_WIDTH.min((screen[0] - 32.0).max(1.0));
        let height = 34.0;
        let gap = 8.0;
        let x = (screen[0] - width) / 2.0;
        let first_y = screen[1] / 2.0 - 18.0;
        return [
            UiRect::new(x, first_y, width, height),
            UiRect::new(x, first_y + height + gap, width, height),
        ];
    }
    let x = (screen[0] - DEATH_BUTTON_WIDTH) / 2.0;
    let first_y = screen[1] / 2.0 + 34.0;
    [
        UiRect::new(x, first_y, DEATH_BUTTON_WIDTH, DEATH_BUTTON_HEIGHT),
        UiRect::new(
            x,
            first_y + DEATH_BUTTON_HEIGHT + DEATH_BUTTON_GAP,
            DEATH_BUTTON_WIDTH,
            DEATH_BUTTON_HEIGHT,
        ),
    ]
}

/// Draws the modal death screen over the world and HUD.
pub fn draw_death_screen(frame: &mut UiFrame, ui: &UiState, screen: [f32; 2]) {
    draw_death_screen_custom(
        frame,
        ui,
        ui.death_cause.as_deref(),
        ["Respawn", "Save & quit"],
        DEATH_NOTE,
        "Enter/Click select   Up/Down choose   Esc focus buttons   F3 debug",
        screen,
    );
}

/// Draws the death screen with caller-provided button labels and wording.
///
/// The network client reuses the same geometry, colours and keyboard focus as
/// single-player but offers `Disconnect` instead of `Save & quit` and shows the
/// server's kill cause.
pub fn draw_death_screen_custom(
    frame: &mut UiFrame,
    ui: &UiState,
    cause: Option<&str>,
    labels: [&str; 2],
    note: &str,
    hint: &str,
    screen: [f32; 2],
) {
    let [width, height] = screen;
    frame.rect(0.0, 0.0, width, height, [0.16, 0.01, 0.02, 0.68]);
    frame.rect(0.0, 0.0, width, height, [0.35, 0.02, 0.03, 0.20]);
    let centre_x = width / 2.0;
    let centre_y = height / 2.0;
    let compact = height < 500.0;
    let (title_y, title_scale, cause_y, cause_scale, note_y, note_scale) = if compact {
        (centre_y - 112.0, 3.2, centre_y - 76.0, 1.1, centre_y - 56.0, 1.3)
    } else {
        (centre_y - 142.0, 4.0, centre_y - 104.0, 1.2, centre_y - 84.0, 1.5)
    };
    frame.title_text_centered(centre_x, title_y, title_scale, [1.0, 0.88, 0.86, 1.0], "You died");
    if let Some(cause) = cause {
        let max_chars = ((width - 32.0) / ob_render::glyph_advance(cause_scale)).max(8.0) as usize;
        frame.text_centered(
            centre_x,
            cause_y,
            cause_scale,
            [0.98, 0.74, 0.66, 1.0],
            &shorten(cause, max_chars),
        );
    }
    frame.text_centered(centre_x, note_y, note_scale, [0.95, 0.82, 0.80, 1.0], note);
    let rects = death_button_rects(screen);
    for (index, (rect, label)) in rects.iter().zip(labels).enumerate() {
        let active = index == ui.death_selected || ui.death_hovered == Some(index);
        let background = if active {
            [0.30, 0.06, 0.07, 0.98]
        } else {
            [0.17, 0.03, 0.04, 0.96]
        };
        frame.rect(rect.x, rect.y, rect.w, rect.h, background);
        let border = if active {
            [0.98, 0.76, 0.38, 1.0]
        } else {
            [0.50, 0.22, 0.22, 1.0]
        };
        frame.outline(rect.x, rect.y, rect.w, rect.h, if active { 2.0 } else { 1.0 }, border);
        let label_scale = if compact { 1.6 } else { 2.0 };
        let label_height = ob_render::line_height(label_scale);
        frame.text_centered(
            rect.center().0,
            rect.y + (rect.h - label_height) / 2.0,
            label_scale,
            [0.96, 0.92, 0.90, 1.0],
            label,
        );
    }
    let hint_scale = if compact { 1.0 } else { 1.2 };
    let max_chars = ((width - 32.0) / ob_render::glyph_advance(hint_scale)).max(8.0) as usize;
    frame.text_centered(
        centre_x,
        death_hint_y(screen),
        hint_scale,
        [0.86, 0.72, 0.72, 1.0],
        &shorten(hint, max_chars),
    );
}

/// Y of the death-screen key hint, clamped above the health row at small sizes.
pub fn death_hint_y(screen: [f32; 2]) -> f32 {
    let rects = death_button_rects(screen);
    let compact = screen[1] < 500.0;
    let mut y = rects[1].y + rects[1].h + if compact { 6.0 } else { 26.0 };
    let (hearts, _) = vitals_layout(screen);
    if let Some(first) = hearts.first() {
        let hint_scale = if compact { 1.0 } else { 1.2 };
        y = y.min(first.y - ob_render::line_height(hint_scale) - 4.0);
    }
    y
}

/// Facing label from a yaw in radians (0 faces -Z / north).
pub fn facing_label(yaw: f32) -> &'static str {
    let turns = yaw.rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU;
    let sector = ((turns * 8.0).round() as i32).rem_euclid(8);
    // Positive yaw rotates from north (-Z) towards west (-X).
    match sector {
        0 => "north (-Z)",
        1 => "north-west",
        2 => "west (-X)",
        3 => "south-west",
        4 => "south (+Z)",
        5 => "south-east",
        6 => "east (+X)",
        _ => "north-east",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_core::config::TextureStyle;

    /// Deterministic stats snapshot shared by the F3 layout tests.
    fn sample_debug_stats() -> DebugStats {
        DebugStats {
            version: "0.0.8".into(),
            fps: 60,
            frame_p50_ms: 16.67,
            frame_p95_ms: 18.02,
            position: [123.456, 78.0, -234.567],
            chunk: [7, 4, -14],
            facing: "north (-Z)",
            target: Some(("openblock:grass".into(), [1, 2, 3])),
            seed: 0,
            render_distance: 6,
            loaded_chunks: 217,
            drawn_sections: 244,
            cached_sections: 1133,
            lod_batches: 37,
            lod_sections: 812,
            lod_skipped_sections: 4100,
            mesh_bytes: 3 * 1024 * 1024,
            triangles: 106398,
            draw_calls: 244,
            mesh_workers: 7,
            mesh_in_flight: 3,
            mesh_completed: 999,
            mesh_backlog: 1,
            heap_bytes: 2 * 1024 * 1024,
            ticks: 1234,
            adapter: "Test GPU (vulkan, radv)".into(),
            window: [1280, 720],
            fov_degrees: 70.0,
            ui_scale: "UI 100% (1280x720 logical)".to_string(),
            time_of_day: 0.5,
            texture_source: "classic 16px".into(),
            ui_font: "Monocraft".into(),
            game_mode: "survival",
            health: 14.0,
            hunger: 12.0,
            held: "Stone Pickaxe".into(),
            held_durability: Some((129, 131)),
            mining_speed: 4.0,
            movement: "walk",
            audio: "test output / 48000 Hz / 2 ch / 10.7 ms".into(),
            network: None,
            mobs_total: 3,
            mobs_passive: 2,
            mobs_hostile: 1,
            day_phase: 6000,
            day: true,
            defense: 5,
            graphics_summary: "High preset | shadows high 2048² ×3 | clouds high | water high | bloom on | FXAA on"
                .into(),
            shadow_summary: "high 2048² ×3 3×3 PCF".into(),
        }
    }

    #[test]
    fn screen_starts_in_playing_with_hud_visible() {
        let state = UiState::default();
        assert_eq!(state.screen, Screen::Playing);
        assert!(state.hud_visible);
        assert!(!state.debug_overlay);
        assert_eq!(state.settings_tab, SettingsTab::Game);
        assert_eq!(state.settings_selected_row, 0);
        assert_eq!(state.settings_rows().len(), 3);
    }

    #[test]
    fn hotbar_has_nine_default_slots_in_creative_order() {
        let mut state = UiState::default();
        assert_eq!(HOTBAR_SLOTS, 9);
        assert_eq!(state.hotbar, DEFAULT_HOTBAR);
        assert_eq!(
            state.hotbar,
            [
                "openblock:grass",
                "openblock:dirt",
                "openblock:stone",
                "openblock:sand",
                "openblock:log",
                "openblock:leaves",
                "openblock:water",
                "openblock:bedrock",
                "openblock:snow",
            ]
        );
        let registry = Registry::base();
        assert_eq!(
            state.hotbar_block(&registry, 0).map(|block| block.id),
            Some("openblock:grass")
        );
        assert_eq!(
            state.hotbar_block(&registry, 8).map(|block| block.id),
            Some("openblock:snow")
        );
        assert_eq!(state.hotbar_label(2), Some("stone"));
        state.hotbar[1] = "openblock:not_a_block";
        assert!(state.hotbar_block(&registry, 1).is_none());
        assert!(state.hotbar_block(&registry, HOTBAR_SLOTS).is_none());
    }

    #[test]
    fn hotbar_cycling_wraps_across_all_nine_slots() {
        let mut state = UiState::default();
        for expected in 1..HOTBAR_SLOTS {
            state.cycle_hotbar(1);
            assert_eq!(state.hotbar_selected, expected);
        }
        state.cycle_hotbar(1);
        assert_eq!(state.hotbar_selected, 0);
        state.cycle_hotbar(-1);
        assert_eq!(state.hotbar_selected, HOTBAR_SLOTS - 1);
        state.cycle_hotbar(HOTBAR_SLOTS as i32 + 2);
        assert_eq!(state.hotbar_selected, 1);
    }

    #[test]
    fn inventory_screen_toggles_without_opening_settings() {
        let mut state = UiState::default();
        state.toggle_inventory();
        assert_eq!(state.screen, Screen::Inventory);
        assert_ne!(state.screen, Screen::Settings);
        assert!(state.inventory_layout([1280.0, 720.0], 14).slots.len() == 14);
        state.toggle_inventory();
        assert_eq!(state.screen, Screen::Playing);

        state.open_inventory();
        state.close_inventory();
        assert_eq!(state.screen, Screen::Playing);

        // Settings is reachable only from gameplay; the inventory is unaffected.
        state.open_inventory();
        state.open_settings();
        assert_eq!(state.screen, Screen::Inventory);
        state.close_inventory();
        state.open_settings();
        state.toggle_inventory();
        assert_eq!(state.screen, Screen::Settings);
    }

    #[test]
    fn hit_marker_expires_and_death_cause_is_optional() {
        let mut state = UiState::default();
        assert_eq!(state.hit_marker_seconds, 0.0);
        state.flash_mob_hit();
        assert!(state.hit_marker_seconds > 0.0);
        state.tick(0.1);
        assert!(state.hit_marker_seconds > 0.0);
        state.tick(1.0);
        assert_eq!(state.hit_marker_seconds, 0.0);

        state.open_death();
        assert_eq!(state.death_cause, None, "open_death resets the cause");
        state.death_cause = Some("Cause: lost all health".into());
        state.clear_death();
        assert_eq!(state.screen, Screen::Playing);
        assert_eq!(state.death_cause, None);
    }

    #[test]
    fn break_flash_expires_with_the_ui_clock() {
        let mut state = UiState::default();
        state.flash_broken_block("Log");
        assert_eq!(state.break_flash.as_deref(), Some("Log"));
        assert!(state.break_flash_seconds > 0.0);
        state.tick(1.0);
        assert!(state.break_flash_seconds > 0.0);
        state.tick(10.0);
        assert_eq!(state.break_flash_seconds, 0.0);
        assert_eq!(state.break_flash, None);
    }

    #[test]
    fn escape_flow_opens_settings_directly_and_returns_to_playing() {
        let mut state = UiState::default();
        state.open_settings();
        assert_eq!(state.screen, Screen::Settings);
        assert_eq!(state.settings_tab, SettingsTab::Game);
        assert_eq!(state.settings_selected_row, 0);
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::GameMode));

        state.settings_select_tab(SettingsTab::Display);
        state.settings_row_down();
        state.close_settings();
        assert_eq!(state.screen, Screen::Playing);

        // Reopening always lands on the Game tab and its first row.
        state.open_settings();
        assert_eq!(state.settings_tab, SettingsTab::Game);
        assert_eq!(state.settings_selected_row, 0);
    }

    #[test]
    fn death_screen_keeps_priority_over_settings() {
        let mut state = UiState::default();
        state.open_settings();
        state.open_death();
        assert_eq!(state.screen, Screen::Death);
        state.open_settings();
        assert_eq!(state.screen, Screen::Death, "death must not yield to settings");
        state.clear_death();
        assert_eq!(state.screen, Screen::Playing);

        state.open_inventory();
        state.open_settings();
        assert_eq!(state.screen, Screen::Inventory, "settings needs gameplay");
        state.close_inventory();
        state.open_settings();
        state.open_death();
        assert_eq!(state.screen, Screen::Death);
        assert!(state.settings_rows().len() == 3, "tab state is retained");
    }

    #[test]
    fn settings_tabs_cycle_with_tab_and_shift_tab() {
        let mut state = UiState::default();
        state.open_settings();
        assert_eq!(state.settings_tab, SettingsTab::Game);
        state.settings_selected_row = 2;
        state.settings_tab_next();
        assert_eq!(state.settings_tab, SettingsTab::Display);
        assert_eq!(state.settings_selected_row, 0, "tab changes reset the row");
        state.settings_tab_next();
        assert_eq!(state.settings_tab, SettingsTab::Audio);
        state.settings_tab_next();
        assert_eq!(state.settings_tab, SettingsTab::Controls);
        state.settings_tab_next();
        assert_eq!(state.settings_tab, SettingsTab::Textures);
        state.settings_tab_next();
        assert_eq!(state.settings_tab, SettingsTab::Graphics);
        state.settings_tab_next();
        assert_eq!(state.settings_tab, SettingsTab::Game);
        state.settings_tab_prev();
        assert_eq!(state.settings_tab, SettingsTab::Graphics);
        state.settings_tab_prev();
        assert_eq!(state.settings_tab, SettingsTab::Textures);
        state.settings_tab_prev();
        assert_eq!(state.settings_tab, SettingsTab::Controls);
        state.settings_tab_prev();
        assert_eq!(state.settings_tab, SettingsTab::Audio);
        assert_eq!(state.settings_tab, SettingsTab::ALL[2]);
        assert_eq!(SettingsTab::from_name("audio"), Some(SettingsTab::Audio));
        assert_eq!(SettingsTab::from_name("AUDIO"), Some(SettingsTab::Audio));
        assert_eq!(SettingsTab::from_name("nope"), None);
    }

    #[test]
    fn settings_row_navigation_wraps_through_buttons() {
        let mut state = UiState::default();
        state.open_settings();
        assert_eq!(state.settings_focus_count(), 5);
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::GameMode));
        state.settings_row_up();
        assert_eq!(state.settings_selected_row, 4);
        assert_eq!(state.settings_focused_row(), None);
        assert_eq!(state.settings_focused_button(), Some(SettingsButton::SaveAndQuit));
        state.settings_row_down();
        assert_eq!(state.settings_selected_row, 0);
        assert_eq!(state.settings_focused_button(), None);

        state.settings_select_tab(SettingsTab::Display);
        assert_eq!(state.settings_focus_count(), 12);
        state.settings_selected_row = 11;
        assert_eq!(state.settings_focused_button(), Some(SettingsButton::SaveAndQuit));
        state.settings_row_down();
        assert_eq!(state.settings_selected_row, 0);
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::RenderDistance));

        state.settings_select_tab(SettingsTab::Audio);
        assert_eq!(state.settings_rows().len(), 4);
        assert_eq!(state.settings_focus_count(), 6);
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::AudioEnabled));
        state.settings_row_down();
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::MasterVolume));

        state.settings_select_tab(SettingsTab::Controls);
        assert_eq!(state.settings_rows().len(), Action::ALL.len() + 1);
        assert_eq!(state.settings_focus_count(), Action::ALL.len() + 3);
        assert_eq!(
            state.settings_focused_row(),
            Some(SettingsRow::Keybind(Action::MoveForward))
        );
        state.settings_row_up();
        assert_eq!(state.settings_focused_button(), Some(SettingsButton::SaveAndQuit));
        state.settings_row_down();
        assert_eq!(
            state.settings_focused_row(),
            Some(SettingsRow::Keybind(Action::MoveForward))
        );
    }

    #[test]
    fn render_distance_row_shows_chunks_and_blocks() {
        let mut state = UiState::default();
        let info = SettingsInfo::default();
        state.settings.render_distance_chunks = 32;
        assert_eq!(
            state.settings_row_value(SettingsRow::RenderDistance, &info),
            "32 chunks / 512 blocks"
        );
        state.settings.render_distance_chunks = 2;
        assert_eq!(
            state.settings_row_value(SettingsRow::RenderDistance, &info),
            "2 chunks / 32 blocks"
        );
        // The row clamps at the 512-block ceiling.
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        state.settings_selected_row = 0;
        state.settings.render_distance_chunks = SETTINGS_MAX_RENDER_DISTANCE;
        assert_eq!(state.settings_adjust(1), None);
        assert_eq!(state.settings.render_distance_chunks, SETTINGS_MAX_RENDER_DISTANCE);
    }

    #[test]
    fn settings_adjust_clamps_render_distance() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        assert_eq!(state.settings.render_distance_chunks, 6);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::RenderDistance));
        assert_eq!(state.settings.render_distance_chunks, 7);
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::RenderDistance));
        assert_eq!(state.settings.render_distance_chunks, 6);

        state.settings.render_distance_chunks = SETTINGS_MAX_RENDER_DISTANCE;
        assert_eq!(state.settings_adjust(1), None);
        assert_eq!(state.settings.render_distance_chunks, SETTINGS_MAX_RENDER_DISTANCE);
        state.settings.render_distance_chunks = SETTINGS_MIN_RENDER_DISTANCE;
        assert_eq!(state.settings_adjust(-1), None);
        assert_eq!(state.settings.render_distance_chunks, SETTINGS_MIN_RENDER_DISTANCE);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::RenderDistance));
        assert_eq!(state.settings.render_distance_chunks, 3);
    }

    #[test]
    fn settings_adjust_clamps_fov() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        state.settings_selected_row = 1;
        assert_eq!(state.settings.fov_degrees, 70.0);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::Fov));
        assert_eq!(state.settings.fov_degrees, 75.0);
        state.settings.fov_degrees = SETTINGS_MAX_FOV_DEGREES;
        assert_eq!(state.settings_adjust(1), None);
        state.settings.fov_degrees = SETTINGS_MIN_FOV_DEGREES;
        assert_eq!(state.settings_adjust(-1), None);
        state.settings.fov_degrees = 55.0;
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::Fov));
        assert_eq!(state.settings.fov_degrees, SETTINGS_MIN_FOV_DEGREES);
        assert_eq!(state.settings_adjust(-1), None);
    }

    #[test]
    fn settings_adjust_clamps_mouse_sensitivity() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        state.settings_selected_row = 2;
        assert_eq!(state.settings.mouse_sensitivity, 1.0);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::MouseSensitivity));
        assert!((state.settings.mouse_sensitivity - 1.1).abs() < 1e-6);
        state.settings.mouse_sensitivity = SETTINGS_MAX_MOUSE_SENSITIVITY;
        assert_eq!(state.settings_adjust(1), None);
        state.settings.mouse_sensitivity = SETTINGS_MIN_MOUSE_SENSITIVITY;
        assert_eq!(state.settings_adjust(-1), None);
        state.settings.mouse_sensitivity = 0.5;
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::MouseSensitivity));
        assert!((state.settings.mouse_sensitivity - 0.4).abs() < 1e-6);
    }

    #[test]
    fn settings_adjust_cycles_game_mode_and_texture_style() {
        let mut state = UiState::default();
        state.open_settings();
        assert_eq!(state.settings.game_mode, GameMode::Survival);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::GameMode));
        assert_eq!(state.settings.game_mode, GameMode::Creative);
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::GameMode));
        assert_eq!(state.settings.game_mode, GameMode::Survival);

        state.settings_select_tab(SettingsTab::Textures);
        assert_eq!(state.settings.texture_style, TextureStyle::Classic);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::TextureStyle));
        assert_eq!(state.settings.texture_style, TextureStyle::Painterly);
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::TextureStyle));
        assert_eq!(state.settings.texture_style, TextureStyle::Classic);
        state.settings.texture_style = TextureStyle::Minimal;
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::TextureStyle));
        assert_eq!(state.settings.texture_style, TextureStyle::Classic);
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::TextureStyle));
        assert_eq!(state.settings.texture_style, TextureStyle::Minimal);
    }

    #[test]
    fn settings_adjust_audio_toggle_and_volumes() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Audio);
        assert_eq!(
            state.settings_rows(),
            vec![
                SettingsRow::AudioEnabled,
                SettingsRow::MasterVolume,
                SettingsRow::SfxVolume,
                SettingsRow::MusicVolume
            ]
        );
        assert!(state.settings.audio_enabled);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::AudioEnabled));
        assert!(!state.settings.audio_enabled);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::AudioEnabled));
        assert!(state.settings.audio_enabled);

        state.settings_selected_row = 1;
        let info = SettingsInfo::default();
        assert_eq!(state.settings_row_value(SettingsRow::MasterVolume, &info), "80%");
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::MasterVolume));
        assert!((state.settings.master_volume - 0.85).abs() < 1e-6);
        assert_eq!(state.settings_row_value(SettingsRow::MasterVolume, &info), "85%");
        state.settings.master_volume = 1.0;
        assert_eq!(state.settings_adjust(1), None);
        assert!(!state.settings_can_adjust(SettingsRow::MasterVolume, 1));
        state.settings.master_volume = 0.0;
        assert_eq!(state.settings_adjust(-1), None);
        assert!(!state.settings_can_adjust(SettingsRow::MasterVolume, -1));
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::MasterVolume));
        assert!((state.settings.master_volume - 0.05).abs() < 1e-6);

        state.settings_selected_row = 2;
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::SfxVolume));
        assert!((state.settings.sfx_volume - 0.95).abs() < 1e-6);
        state.settings_selected_row = 3;
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::MusicVolume));
        assert!((state.settings.music_volume - 0.5).abs() < 1e-6);
        assert_eq!(state.settings_row_value(SettingsRow::MusicVolume, &info), "50%");

        // Persisted non-finite values display and adjust from their defaults.
        state.settings.master_volume = f32::NAN;
        assert_eq!(state.settings_row_value(SettingsRow::MasterVolume, &info), "80%");
        assert!(state.settings_row_fraction(SettingsRow::MasterVolume).is_some());
        state.settings.master_volume = 0.8;
        let screen = [1280.0, 720.0];
        let arrows = state.settings_arrow_rects(screen);
        assert_eq!(arrows.len(), 4);
        assert!(arrows[0].is_none(), "sound is a toggle");
        for arrows in &arrows[1..] {
            assert!(arrows.is_some(), "every volume row has arrows");
        }
    }

    #[test]
    fn settings_adjust_toggles_checkboxes_and_skips_read_only_rows() {
        let mut state = UiState::default();
        state.open_settings();
        assert!(state.hud_visible);
        state.settings_selected_row = 1;
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::ShowHud));
        assert!(!state.hud_visible);
        assert!(!state.settings.show_hud, "the HUD preference is persisted");
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::ShowHud));
        assert!(state.hud_visible);
        assert!(state.settings.show_hud);

        state.settings_selected_row = 2;
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::DebugOverlay));
        assert!(state.settings.show_debug_overlay);
        assert!(state.debug_overlay);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::DebugOverlay));
        assert!(!state.settings.show_debug_overlay);
        assert!(!state.debug_overlay);

        state.settings_select_tab(SettingsTab::Display);
        state.settings_selected_row = 3;
        assert!(!state.settings.fullscreen);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::Fullscreen));
        assert!(state.settings.fullscreen);
        state.settings_selected_row = 4;
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::UiScale));
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::UiScale));
        assert_eq!(state.settings.ui_scale_percent, 125);
        state.settings_selected_row = 5;
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::ReducedMotion));
        assert!(!state.settings.reduced_motion);
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::ReducedMotion));
        assert!(state.settings.reduced_motion);
        state.settings_selected_row = 6;
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::MeshWorkers));
        assert_eq!(state.settings_adjust(1), None);
        assert_eq!(state.settings_activate(), None);
    }

    #[test]
    fn settings_activate_handles_rows_and_footer_buttons() {
        let mut state = UiState::default();
        state.open_settings();
        assert_eq!(
            state.settings_activate(),
            Some(SettingsAction::Adjust(SettingsRow::GameMode))
        );
        assert_eq!(state.settings.game_mode, GameMode::Creative);

        state.settings_select_tab(SettingsTab::Display);
        state.settings_selected_row = 3;
        assert_eq!(
            state.settings_activate(),
            Some(SettingsAction::Adjust(SettingsRow::Fullscreen))
        );
        assert!(state.settings.fullscreen);

        state.settings_select_tab(SettingsTab::Textures);
        state.settings_selected_row = 1;
        assert_eq!(state.settings_activate(), None, "no pack is available");
        state.active_texture_pack = Some("rotrBLOCKS V88".into());
        assert_eq!(
            state.settings_activate(),
            Some(SettingsAction::Adjust(SettingsRow::TexturePack))
        );
        // The client owns the renderer switch; the UI only requests it.
        assert_eq!(state.settings.texture_pack, None);

        state.settings_selected_row = 3;
        assert_eq!(state.settings_activate(), Some(SettingsAction::SaveAndQuit));
        assert_eq!(state.screen, Screen::Settings);
        state.settings_selected_row = 2;
        assert_eq!(state.settings_activate(), Some(SettingsAction::Resume));
        assert_eq!(state.screen, Screen::Playing);
    }

    #[test]
    fn settings_click_switches_tabs_and_activates_rows_and_buttons() {
        let mut state = UiState::default();
        state.open_settings();
        let screen = [1280.0, 720.0];
        let tabs = settings_tab_rects(screen);
        state.cursor = tabs[1].center();
        assert_eq!(state.settings_click(screen), None);
        assert_eq!(state.settings_tab, SettingsTab::Display);

        let rows = state.settings_row_rects(screen);
        assert_eq!(rows.len(), 10);
        state.cursor = rows[3].center();
        assert_eq!(
            state.settings_click(screen),
            Some(SettingsAction::Adjust(SettingsRow::Fullscreen))
        );
        assert!(state.settings.fullscreen);

        let buttons = settings_button_rects(screen);
        state.cursor = buttons[0].center();
        assert_eq!(state.settings_click(screen), Some(SettingsAction::Resume));
        assert_eq!(state.screen, Screen::Playing);

        state.open_settings();
        state.cursor = (5.0, 5.0);
        assert_eq!(state.settings_click(screen), None);
        assert_eq!(state.screen, Screen::Settings, "outside clicks do nothing");
    }

    #[test]
    fn settings_hover_selects_rows_and_buttons() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        let screen = [1280.0, 720.0];
        let rows = state.settings_row_rects(screen);
        state.cursor = rows[2].center();
        state.settings_hover(screen);
        assert_eq!(state.settings_hovered_row, Some(2));
        assert_eq!(state.settings_selected_row, 2);

        let tabs = settings_tab_rects(screen);
        state.cursor = tabs[0].center();
        state.settings_hover(screen);
        assert_eq!(state.settings_hovered_tab, Some(SettingsTab::Game));
        assert_eq!(state.settings_hovered_row, None);
        assert_eq!(state.settings_selected_row, 2, "tab hover keeps row focus");

        let buttons = settings_button_rects(screen);
        state.cursor = buttons[1].center();
        state.settings_hover(screen);
        assert_eq!(state.settings_hovered_button, Some(1));
        assert_eq!(state.settings_selected_row, rows.len() + 1);

        state.cursor = (5.0, 5.0);
        state.settings_hover(screen);
        assert_eq!(state.settings_hovered_tab, None);
        assert_eq!(state.settings_hovered_row, None);
        assert_eq!(state.settings_hovered_button, None);
    }

    #[test]
    fn settings_panel_tabs_rows_and_buttons_fit_inside() {
        let mut state = UiState::default();
        state.open_settings();
        let screen = [1280.0, 720.0];
        let panel = settings_panel_rect(screen);
        assert!((panel.x + panel.w / 2.0 - 640.0).abs() < 0.01);
        assert!((panel.y + panel.h / 2.0 - 360.0).abs() < 0.01);
        assert!(panel.w <= screen[0] && panel.h <= screen[1]);
        let tabs = settings_tab_rects(screen);
        assert_eq!(tabs.len(), SettingsTab::ALL.len());
        for tab in tabs {
            assert!(tab.x >= panel.x && tab.x + tab.w <= panel.x + panel.w);
            assert!(tab.y >= panel.y && tab.y + tab.h <= panel.y + panel.h);
        }
        // Every tab label fits inside its own tab at the resolved scale.
        for (tab_rect, tab) in tabs.iter().zip(SettingsTab::ALL.iter()) {
            let scale = settings_tab_label_scale(*tab_rect, tab.name(), SETTINGS_METRICS_BASE.tab_scale);
            let width = tab.name().chars().count() as f32 * ob_render::GLYPH_SIZE * scale;
            assert!(
                width <= tab_rect.w + 0.01,
                "{} label is {width:.1} px wide in a {:.1} px tab",
                tab.name(),
                tab_rect.w
            );
        }
        let buttons = settings_button_rects(screen);
        for button in buttons {
            assert!(button.x >= panel.x && button.x + button.w <= panel.x + panel.w);
            assert!(button.y + button.h <= panel.y + panel.h - SETTINGS_FOOTER_Y_FROM_BOTTOM);
        }
        for tab in SettingsTab::ALL {
            state.settings_select_tab(tab);
            let rows = state.settings_row_rects(screen);
            assert_eq!(rows.len(), state.settings_rows().len());
            for row in rows {
                assert!(row.x >= panel.x && row.x + row.w <= panel.x + panel.w);
                assert!(row.y + row.h <= buttons[0].y, "rows must clear the buttons");
            }
        }
    }

    #[test]
    fn settings_rows_have_minimum_breathing_room_on_every_tab() {
        let mut state = UiState::default();
        state.open_settings();
        for screen in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            let panel = settings_panel_rect(screen);
            for tab in SettingsTab::ALL {
                state.settings_select_tab(tab);
                let rows = state.settings_row_rects(screen);
                for pair in rows.windows(2) {
                    if pair[1].y <= pair[0].y {
                        // Two-column Controls layout: the next entry starts
                        // back at the top of the second column.
                        continue;
                    }
                    let pitch = pair[1].y - pair[0].y;
                    assert!(
                        pitch >= ob_render::line_height(1.5) + 4.0,
                        "{tab:?} rows at {screen:?} are too close: {pitch}"
                    );
                    assert!(
                        pair[1].y - (pair[0].y + pair[0].h) >= 6.0,
                        "{tab:?} hit boxes need a visible gap at {screen:?}"
                    );
                }
                let buttons = settings_button_rects(screen);
                for row in rows {
                    assert!(row.x >= panel.x && row.x + row.w <= panel.x + panel.w);
                    assert!(row.y >= panel.y && row.y + row.h <= panel.y + panel.h);
                    assert!(row.y + row.h <= buttons[0].y, "rows must clear the buttons");
                }
            }
            // Tabs, hints and buttons must also fit at the smaller target.
            let tabs = settings_tab_rects(screen);
            for tab in tabs {
                assert!(tab.x >= panel.x && tab.x + tab.w <= panel.x + panel.w);
                assert!(tab.y >= panel.y && tab.y + tab.h <= panel.y + panel.h);
            }
            let buttons = settings_button_rects(screen);
            for button in buttons {
                assert!(button.y + button.h <= panel.y + panel.h);
            }
        }
        assert!(SETTINGS_CONTROLS_ROW_STEP >= ob_render::line_height(1.1) + 4.0);
        let controls_rows = Action::ALL.len().div_ceil(2);
        let controls_note_bottom = SETTINGS_CONTENT_TOP
            + (controls_rows - 1) as f32 * SETTINGS_CONTROLS_ROW_STEP
            + SETTINGS_CONTROLS_ROW_HEIGHT
            + 16.0;
        assert!(
            controls_note_bottom <= SETTINGS_PANEL_HEIGHT - 116.0,
            "the Controls note must clear the footer buttons"
        );
    }

    #[test]
    fn settings_arrow_hit_boxes_are_separate_and_hit_testable() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        let screen = [1280.0, 720.0];
        let rows = state.settings_row_rects(screen);
        let arrows = state.settings_arrow_rects(screen);
        assert_eq!(rows.len(), 10);
        assert!(arrows[0].is_some(), "render distance is a range");
        assert!(arrows[1].is_some(), "FOV is a range");
        assert!(arrows[2].is_some(), "sensitivity is a range");
        assert!(arrows[3].is_none(), "fullscreen is a toggle");
        assert!(arrows[4].is_some(), "GUI scale is a choice");
        assert!(arrows[5].is_none(), "reduced motion is a toggle");
        assert!(arrows[6].is_none(), "mesh workers is read-only");
        let (left, right) = arrows[0].expect("render distance arrows");
        assert!(left.x + left.w < right.x, "arrows must not touch");
        assert!(right.x + right.w <= rows[0].x + rows[0].w);
        assert_eq!(state.settings_arrow_at(screen, left.center()), Some((0, -1)));
        assert_eq!(state.settings_arrow_at(screen, right.center()), Some((0, 1)));
        assert_eq!(
            state.settings_arrow_at(screen, (left.x - 24.0, left.center().1)),
            None,
            "the row body is not an arrow"
        );
        assert_eq!(state.settings_arrow_at(screen, (1.0, 1.0)), None);
    }

    #[test]
    fn settings_click_left_arrow_decreases_and_row_body_only_focuses() {
        use ob_core::config::GameMode;
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        let screen = [1280.0, 720.0];
        let rows = state.settings_row_rects(screen);
        let (left, right) = state.settings_arrow_rects(screen)[0].expect("render distance arrows");
        assert_eq!(state.settings.render_distance_chunks, 6);
        state.cursor = left.center();
        assert_eq!(
            state.settings_click(screen),
            Some(SettingsAction::Adjust(SettingsRow::RenderDistance))
        );
        assert_eq!(state.settings.render_distance_chunks, 5, "left arrow decrements");
        state.cursor = rows[0].center();
        assert_eq!(state.settings_click(screen), None, "the row body only focuses");
        assert_eq!(state.settings.render_distance_chunks, 5, "body click must not increase");
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::RenderDistance));
        state.cursor = right.center();
        assert_eq!(
            state.settings_click(screen),
            Some(SettingsAction::Adjust(SettingsRow::RenderDistance))
        );
        assert_eq!(state.settings.render_distance_chunks, 6, "right arrow increments");
        state.cursor = rows[6].center();
        assert_eq!(
            state.settings_click(screen),
            None,
            "the read-only mesh-worker row does nothing"
        );

        // Choice rows also only focus on a body click and wrap backwards on `<`.
        state.settings_select_tab(SettingsTab::Game);
        let game_rows = state.settings_row_rects(screen);
        state.cursor = game_rows[0].center();
        assert_eq!(state.settings_click(screen), None);
        assert_eq!(state.settings.game_mode, GameMode::Survival);
        let (left, _) = state.settings_arrow_rects(screen)[0].expect("game mode arrows");
        state.cursor = left.center();
        assert_eq!(
            state.settings_click(screen),
            Some(SettingsAction::Adjust(SettingsRow::GameMode))
        );
        assert_eq!(state.settings.game_mode, GameMode::Creative, "choice wraps backwards");
    }

    #[test]
    fn settings_adjust_minus_one_decrements_every_range_and_clamps() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        state.settings_selected_row = 0;
        assert_eq!(state.settings.render_distance_chunks, 6);
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::RenderDistance));
        assert_eq!(state.settings.render_distance_chunks, 5);
        state.settings.render_distance_chunks = SETTINGS_MIN_RENDER_DISTANCE;
        assert_eq!(state.settings_adjust(-1), None);
        assert!(!state.settings_can_adjust(SettingsRow::RenderDistance, -1));

        state.settings_selected_row = 1;
        assert_eq!(state.settings.fov_degrees, 70.0);
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::Fov));
        assert!((state.settings.fov_degrees - 65.0).abs() < 1e-6);
        state.settings.fov_degrees = SETTINGS_MIN_FOV_DEGREES;
        assert_eq!(state.settings_adjust(-1), None);
        assert!(!state.settings_can_adjust(SettingsRow::Fov, -1));

        state.settings_selected_row = 2;
        assert_eq!(state.settings.mouse_sensitivity, 1.0);
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::MouseSensitivity));
        assert!((state.settings.mouse_sensitivity - 0.9).abs() < 1e-6);
        state.settings.mouse_sensitivity = SETTINGS_MIN_MOUSE_SENSITIVITY;
        assert_eq!(state.settings_adjust(-1), None);
        assert!(!state.settings_can_adjust(SettingsRow::MouseSensitivity, -1));
    }

    #[test]
    fn debug_overlay_fits_every_scaled_release_target() {
        for physical in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            for percent in [75_u16, 100, 125, 150] {
                let screen = crate::ui_scale::ui_screen(physical, percent);
                let mut stats = sample_debug_stats();
                stats.ui_scale = crate::ui_scale::debug_line(percent, physical);
                let layout = debug_overlay_layout(screen, &stats);
                let label = format!("{physical:?} at {percent}%");
                assert!(layout.left.panel.x >= 0.0 && layout.left.panel.y >= 0.0);
                assert!(layout.left.panel.x + layout.left.panel.w <= screen[0] + 0.01, "{label}");
                assert!(layout.left.panel.y + layout.left.panel.h <= screen[1] + 0.01, "{label}");
                if let Some(right) = &layout.right {
                    assert!(right.panel.x + right.panel.w <= screen[0] + 0.01, "{label}");
                    assert!(right.panel.y + right.panel.h <= screen[1] + 0.01, "{label}");
                }
                let hotbar_top = screen[1] - HOTBAR_SLOT_SIZE - 18.0;
                assert!(
                    layout.footer_y + ob_render::line_height(DEBUG_FOOTER_SCALE) <= hotbar_top,
                    "{label} F3 footer overlaps the hotbar"
                );
                let expected = if layout.compact { 10 } else { 16 };
                assert_eq!(layout.left.lines.len(), expected, "{label}");
            }
        }
    }

    #[test]
    fn debug_overlay_switches_to_the_compact_set_on_short_logical_targets() {
        let mut stats = sample_debug_stats();
        stats.ui_scale = crate::ui_scale::debug_line(150, [960.0, 540.0]);
        let screen = crate::ui_scale::ui_screen([960.0, 540.0], 150);
        let layout = debug_overlay_layout(screen, &stats);
        assert!(layout.compact, "640x360 logical needs the compact F3 set");
        assert_eq!(layout.columns, DebugColumns::LeftOnly);
        assert_eq!(layout.left.lines.len(), 10);
        let hotbar_top = screen[1] - HOTBAR_SLOT_SIZE - 18.0;
        assert!(
            layout.left.panel.y + layout.left.panel.h < hotbar_top,
            "the compact overlay must clear the hotbar"
        );
        assert!(
            layout.footer_y + ob_render::line_height(DEBUG_FOOTER_SCALE) < hotbar_top,
            "the compact footer must clear the hotbar"
        );
        let lines = stats.compact_left_lines();
        assert!(lines.iter().any(|line| line.contains("UI 150% (640x360 logical)")));
        assert!(lines.iter().any(|line| line.contains("UI font: Monocraft")));
        assert!(lines.iter().any(|line| line.contains("XYZ")));
        assert!(lines.iter().any(|line| line.contains("FPS")));
        assert!(lines.iter().any(|line| line.contains("Vitals")));
    }

    #[test]
    fn debug_overlay_has_section_spacing_and_never_overlaps_at_1280() {
        let stats = sample_debug_stats();
        let screen = [1280.0, 720.0];
        let layout = debug_overlay_layout(screen, &stats);
        assert_eq!(layout.columns, DebugColumns::SideBySide);
        assert_eq!(layout.left.lines.len(), 16);
        let right = layout.right.as_ref().expect("hardware column");
        assert!(layout.left.panel.x + layout.left.panel.w + DEBUG_COLUMN_GAP <= right.panel.x + 0.01);
        assert!(layout.left.panel.y + layout.left.panel.h <= layout.footer_y - 4.0);
        assert!(right.panel.y + right.panel.h <= layout.footer_y - 4.0);
        for column in [&layout.left, right] {
            assert!(column.panel.x >= DEBUG_MARGIN - 0.01);
            assert!(column.panel.x + column.panel.w <= screen[0] - DEBUG_MARGIN + 0.01);
            for line in &column.lines {
                assert!(line.y >= column.panel.y + DEBUG_PADDING - 0.01);
                assert!(line.y + ob_render::line_height(column.scale) <= column.panel.y + column.panel.h);
                assert!(line.x + line.width <= column.panel.x + column.panel.w + 0.01);
            }
        }
        // Every consecutive pair in a section keeps a real gap; section changes
        // add the section gap on top of it.
        for pair in layout.left.lines.windows(2) {
            let delta = pair[1].y - pair[0].y;
            assert!(delta >= debug_line_step(layout.left.scale) - 0.01);
            assert!(delta >= ob_render::line_height(layout.left.scale) + 6.0);
            if pair[0].section != pair[1].section {
                assert!(delta >= debug_line_step(layout.left.scale) + DEBUG_SECTION_GAP - 0.01);
            }
        }
        assert!(layout.header_separator_y > layout.left.lines[0].y);
        assert!(layout.header_separator_y < layout.left.lines[1].y);
        assert!(debug_line_step(layout.left.scale) >= ob_render::line_height(layout.left.scale) + 6.0);
        assert!(layout.footer_y > layout.left.panel.y + layout.left.panel.h);
        assert!(
            layout.footer_y + ob_render::line_height(DEBUG_FOOTER_SCALE) < screen[1] - 44.0 - 18.0 - 4.0,
            "the footer hint stays above the hotbar"
        );
    }

    #[test]
    fn debug_overlay_fits_readably_at_960x540() {
        let stats = sample_debug_stats();
        let screen = [960.0, 540.0];
        let layout = debug_overlay_layout(screen, &stats);
        assert_ne!(layout.columns, DebugColumns::LeftOnly);
        assert!(layout.left.scale >= 1.0);
        let right = layout.right.as_ref().expect("hardware column is kept");
        match layout.columns {
            DebugColumns::Stacked => {
                assert!(right.panel.y >= layout.left.panel.y + layout.left.panel.h);
            }
            DebugColumns::SideBySide => {
                assert!(layout.left.panel.x + layout.left.panel.w + DEBUG_COLUMN_GAP <= right.panel.x + 0.01);
            }
            DebugColumns::LeftOnly => unreachable!(),
        }
        assert!(right.panel.y + right.panel.h <= layout.footer_y - 4.0);
        assert!(layout.left.panel.x >= DEBUG_MARGIN - 0.01);
        assert!(layout.left.panel.x + layout.left.panel.w <= screen[0] - DEBUG_MARGIN + 0.01);
        assert!(right.panel.x + right.panel.w <= screen[0] - DEBUG_MARGIN + 0.01);
        assert!(
            layout.footer_y + ob_render::line_height(DEBUG_FOOTER_SCALE) < screen[1] - 44.0 - 18.0 - 4.0,
            "the footer hint stays above the hotbar"
        );
        for column in [&layout.left, right] {
            for pair in column.lines.windows(2) {
                let section_change = pair[0].section != pair[1].section;
                let minimum = ob_render::line_height(column.scale) + 6.0;
                if !section_change {
                    assert!(pair[1].y - pair[0].y >= minimum);
                }
            }
        }
    }

    #[test]
    fn debug_overlay_degrades_to_the_left_column_on_tiny_screens() {
        let stats = sample_debug_stats();
        let screen = [640.0, 360.0];
        let layout = debug_overlay_layout(screen, &stats);
        assert_eq!(layout.columns, DebugColumns::LeftOnly);
        assert!(layout.right.is_none());
        assert!(!layout.left.lines.is_empty());
        assert!(layout.left.panel.y + layout.left.panel.h <= screen[1]);
        assert!(layout.footer_y <= screen[1]);
    }

    #[test]
    fn debug_right_column_wraps_long_adapters_without_losing_text() {
        let mut stats = sample_debug_stats();
        stats.adapter = "AMD Ryzen 7 9800X3D 8-Core Processor (RADV RAPHAEL_MENDOCINO) (vulkan, radv)".into();
        let lines = stats.right_lines();
        assert!(
            lines
                .iter()
                .all(|line| line.chars().count() <= DEBUG_RIGHT_WRAP_COLUMNS)
        );
        let joined = lines.join(" ");
        for needle in ["AMD", "Ryzen", "RAPHAEL_MENDOCINO", "vulkan", "radv"] {
            assert!(joined.contains(needle), "wrapping dropped {needle}");
        }
        assert!(lines.iter().any(|line| line == "Window 1280x720"));
    }

    #[test]
    fn settings_texture_pack_row_reflects_source_and_enablement() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Textures);
        state.settings_selected_row = 1;
        let info = SettingsInfo::default();
        assert_eq!(
            state.settings_row_value(SettingsRow::TexturePack, &info),
            "built-in only"
        );
        assert!(!state.settings_row_enabled(SettingsRow::TexturePack));
        assert_eq!(state.settings_adjust(1), None);

        state.settings.texture_pack = Some("/packs/rotrBLOCKS".into());
        assert!(state.settings_row_enabled(SettingsRow::TexturePack));
        assert_eq!(
            state.settings_row_value(SettingsRow::TexturePack, &info),
            "enable saved pack"
        );
        assert_eq!(
            state.settings_adjust(1),
            Some(SettingsRow::TexturePack),
            "the client performs the actual toggle"
        );

        state.active_texture_pack = Some("rotrBLOCKS V88".into());
        assert_eq!(
            state.settings_row_value(SettingsRow::TexturePack, &info),
            "rotrBLOCKS V88"
        );
    }

    #[test]
    fn settings_ui_font_row_shows_the_active_source_and_actions() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        let info = SettingsInfo::default();
        assert_eq!(
            state.settings_row_value(SettingsRow::UiFont, &info),
            "Monocraft (built-in)"
        );
        assert_eq!(
            state.settings_row_info(SettingsRow::UiFont, &info).kind,
            SettingsRowKind::ReadOnly
        );
        assert!(!state.settings_can_adjust(SettingsRow::UiFont, 1));

        state.active_ui_font_name = "custom".to_string();
        state.active_ui_font_source = "Minecraftia.ttf".to_string();
        assert_eq!(
            state.settings_row_value(SettingsRow::UiFont, &info),
            "custom (Minecraftia.ttf)"
        );

        // The two action rows request renderer/persistence side effects.
        let rows = state.settings_rows();
        state.settings_selected_row = rows.iter().position(|row| *row == SettingsRow::ResetUiFont).unwrap();
        assert_eq!(state.settings_activate(), Some(SettingsAction::ResetUiFont));
        state.settings_selected_row = rows.iter().position(|row| *row == SettingsRow::ReloadUiFont).unwrap();
        assert_eq!(state.settings_activate(), Some(SettingsAction::ReloadUiFont));
        let arrows = state.settings_arrow_rects([1280.0, 720.0]);
        for row in [SettingsRow::UiFont, SettingsRow::ResetUiFont, SettingsRow::ReloadUiFont] {
            let index = state
                .settings_rows()
                .iter()
                .position(|candidate| *candidate == row)
                .unwrap();
            assert!(arrows[index].is_none(), "{row:?} has no arrows");
        }
    }

    #[test]
    fn settings_values_round_trip_through_the_config_file() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        state.settings_selected_row = 0;
        state.settings_adjust(1);
        state.settings_selected_row = 1;
        state.settings_adjust(1);
        state.settings_selected_row = 2;
        state.settings_adjust(1);
        state.settings_selected_row = 3;
        state.settings_adjust(1);
        state.settings_select_tab(SettingsTab::Audio);
        state.settings_selected_row = 0;
        state.settings_adjust(1);
        state.settings_selected_row = 1;
        state.settings_adjust(-1);
        state.settings_selected_row = 2;
        state.settings_adjust(-1);
        state.settings_selected_row = 3;
        state.settings_adjust(-1);
        state.settings_select_tab(SettingsTab::Textures);
        state.settings_selected_row = 0;
        state.settings_adjust(1);
        state.settings_select_tab(SettingsTab::Game);
        state.settings_selected_row = 0;
        state.settings_adjust(1);
        state.settings_selected_row = 2;
        state.settings_adjust(1);

        let dir = std::env::temp_dir().join(format!(
            "openblock-settings-roundtrip-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        ob_core::config::save_settings(&dir, &state.settings).expect("save settings");
        let loaded = ob_core::config::load_settings(&dir);
        assert_eq!(loaded, state.settings);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn controls_reference_is_complete_and_laid_out_in_two_columns() {
        assert_eq!(CONTROLS_REFERENCE.len(), 14);
        let columns = controls_reference_columns();
        assert_eq!(columns[0].len(), 7);
        assert_eq!(columns[1].len(), 7);
        let all: Vec<ControlBinding> = columns.into_iter().flatten().collect();
        let keys: Vec<&str> = all.iter().map(|binding| binding.keys).collect();
        let actions: Vec<&str> = all.iter().map(|binding| binding.action).collect();
        for expected in [
            "WASD",
            "Space",
            "Shift",
            "Ctrl",
            "F",
            "LMB (hold)",
            "RMB",
            "1-9 / Wheel",
            "E",
            "C",
            "F3",
            "F1",
            "F2",
            "Esc",
        ] {
            assert!(keys.contains(&expected), "missing key binding {expected}");
        }
        for expected in [
            "Move",
            "Jump",
            "Crouch / sneak",
            "Sprint",
            "Toggle fly",
            "Mine",
            "Place / eat / use",
            "Hotbar slot",
            "Inventory",
            "Debug overlay",
            "HUD",
            "Screenshot",
            "Settings",
        ] {
            assert!(actions.contains(&expected), "missing action {expected}");
        }
        assert!(SETTINGS_PERSISTENCE_NOTE.contains("save automatically"));
        assert!(SETTINGS_HINT_LINES[0].contains("Tab"));
        assert!(SETTINGS_HINT_LINES[0].contains("Up/Down"));
        assert!(SETTINGS_HINT_LINES[0].contains("Left/Right"));
        assert!(SETTINGS_HINT_LINES[0].contains("scroll"));
        assert!(SETTINGS_HINT_LINES[1].contains("< >"));
        assert!(SETTINGS_HINT_LINES[1].contains("Esc"));
    }

    #[test]
    fn settings_screen_draws_geometry_on_every_tab() {
        let mut state = UiState::default();
        state.open_settings();
        let info = SettingsInfo::default();
        for screen in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            for tab in SettingsTab::ALL {
                state.settings_select_tab(tab);
                let mut frame = UiFrame::new(screen[0], screen[1]);
                draw_settings_screen(&mut frame, &state, &info, screen);
                let (vertices, quads) = frame.measure();
                assert!(vertices > 0, "{tab:?} tab must emit geometry at {screen:?}");
                assert!(
                    quads > 40,
                    "{tab:?} tab should draw a full panel at {screen:?}, got {quads} quads"
                );
            }
        }
    }

    #[test]
    fn hotbar_layout_is_centred_and_hit_testable() {
        let state = UiState::default();
        let rects = state.hotbar_rects([1280.0, 720.0]);
        assert_eq!(rects.len(), HOTBAR_SLOTS);
        let first = rects[0];
        let last = rects[HOTBAR_SLOTS - 1];
        let total = last.x + last.w - first.x;
        let centered = (first.x + total / 2.0 - 1280.0 / 2.0).abs();
        assert!(centered < 0.01, "hotbar must be centred");
        assert!(first.contains(first.x + 1.0, first.y + 1.0));
        assert!(!first.contains(-10.0, -10.0));
    }

    #[test]
    fn selecting_a_hotbar_slot_shows_the_block_name() {
        let mut state = UiState::default();
        state.select_hotbar(3);
        assert_eq!(state.hotbar_selected, 3);
        assert!(state.block_name_seconds > 0.0);
        state.tick(10.0);
        assert_eq!(state.block_name_seconds, 0.0);
        state.select_hotbar(HOTBAR_SLOTS + 2);
        assert_eq!(state.hotbar_selected, 2);
    }

    #[test]
    fn debug_lines_cover_the_important_fields() {
        let stats = DebugStats {
            version: "0.0.3".into(),
            fps: 120,
            frame_p50_ms: 8.0,
            frame_p95_ms: 16.0,
            position: [1.0, 2.0, 3.0],
            chunk: [0, 0, 0],
            facing: "north (-Z)",
            target: Some(("openblock:grass".into(), [1, 2, 3])),
            seed: 42,
            render_distance: 8,
            loaded_chunks: 217,
            drawn_sections: 244,
            cached_sections: 1133,
            lod_batches: 37,
            lod_sections: 812,
            lod_skipped_sections: 4100,
            mesh_bytes: 3 * 1024 * 1024,
            triangles: 106398,
            draw_calls: 244,
            mesh_workers: 7,
            mesh_in_flight: 3,
            mesh_completed: 999,
            mesh_backlog: 1,
            heap_bytes: 2 * 1024 * 1024,
            ticks: 1234,
            adapter: "Test GPU".into(),
            window: [1280, 720],
            fov_degrees: 70.0,
            ui_scale: "UI 100% (1280x720 logical)".to_string(),
            time_of_day: 0.5,
            texture_source: "painterly 32px".into(),
            ui_font: "Monocraft".into(),
            game_mode: "survival",
            health: 14.0,
            hunger: 12.5,
            movement: "sprint",
            held: "Stone Pickaxe".into(),
            held_durability: Some((129, 131)),
            mining_speed: 4.0,
            audio: "headless / 48000 Hz / 2 ch".into(),
            network: None,
            mobs_total: 0,
            mobs_passive: 0,
            mobs_hostile: 0,
            day_phase: 18000,
            day: false,
            defense: 0,
            graphics_summary: "High preset | shadows high | clouds high | water high | bloom on | FXAA on".into(),
            shadow_summary: "high 2048² ×3 3×3 PCF".into(),
        };
        let left = stats.left_lines();
        assert_eq!(left.len(), 16);
        assert!(left.iter().any(|line| line == "Held: Stone Pickaxe 129/131  speed 4.0"));
        let joined = left.join("\n");
        for needle in [
            "120 FPS",
            "Game: survival",
            "Vitals: 14.0 / 20 HP, 12.5 / 20 hunger",
            "Move: sprint",
            "Chunk",
            "Facing north",
            "Textures: painterly 32px",
            "Seed 42",
            "UI 100% (1280x720 logical)",
            "Target openblock:grass",
            "Mesh workers 7",
            "Heap 2.0 MiB",
            "Ticks 1234",
        ] {
            assert!(joined.contains(needle), "missing {needle} in:\n{joined}");
        }
        let right = stats.right_lines();
        assert!(right[0].contains("Test GPU"));
        assert!(right[1].contains("1280x720"));
    }

    #[test]
    fn held_line_reports_new_iron_tool_durability_and_speed() {
        let mut stats = sample_debug_stats();
        stats.held = "Iron Pickaxe".into();
        stats.held_durability = Some((251, 251));
        stats.mining_speed = 6.0;
        assert_eq!(stats.held_line(), "Held: Iron Pickaxe 251/251  speed 6.0");
        assert!(
            stats
                .left_lines()
                .iter()
                .any(|line| line == "Held: Iron Pickaxe 251/251  speed 6.0")
        );
        // Non-tool/armor holds keep a plain name with no durability suffix.
        stats.held = "Cooked Beef".into();
        stats.held_durability = None;
        stats.mining_speed = 1.0;
        assert_eq!(stats.held_line(), "Held: Cooked Beef  speed 1.0");
        stats.held.clear();
        assert_eq!(stats.held_line(), "Held: empty  speed 1.0");
    }

    #[test]
    fn facing_labels_cover_all_eight_directions() {
        use std::f32::consts::TAU;
        let directions: Vec<&str> = (0..8).map(|i| facing_label(i as f32 * TAU / 8.0)).collect();
        assert_eq!(directions[0], "north (-Z)");
        assert_eq!(directions[1], "north-west");
        assert_eq!(directions[2], "west (-X)");
        assert_eq!(directions[4], "south (+Z)");
        assert_eq!(directions[6], "east (+X)");
        assert_eq!(facing_label(TAU), "north (-Z)");
        assert_eq!(facing_label(-TAU / 4.0), "east (+X)");
    }

    #[test]
    fn settings_panel_is_620_by_520_and_centred() {
        let panel = settings_panel_rect([1280.0, 720.0]);
        let centre = panel.center();
        assert!((centre.0 - 640.0).abs() < 0.01);
        assert!((centre.1 - 360.0).abs() < 0.01);
        assert_eq!(panel.w, SETTINGS_PANEL_WIDTH);
        assert_eq!(panel.h, SETTINGS_PANEL_HEIGHT);
        assert_eq!((panel.w, panel.h), (620.0, 520.0));
    }

    #[test]
    fn armor_pips_sit_next_to_the_hunger_row_and_stay_on_screen() {
        let screen = [1280.0, 720.0];
        let pips = armor_layout(screen);
        assert_eq!(pips.len(), ARMOR_ICON_COUNT);
        let (_, hunger) = vitals_layout(screen);
        assert_eq!(pips[0].y, hunger[0].y);
        assert!(pips[0].x > hunger[VITAL_ICON_COUNT - 1].x);
        for pair in pips.windows(2) {
            assert!(pair[1].x - (pair[0].x + pair[0].w) >= VITAL_ICON_GAP - 0.01);
        }
        assert!(pips[ARMOR_ICON_COUNT - 1].x + pips[ARMOR_ICON_COUNT - 1].w < screen[0]);
        let mut frame = UiFrame::new(screen[0], screen[1]);
        draw_armor(&mut frame, screen, 15);
        assert_eq!(ARMOR_POINTS_PER_ICON, 2.0);
    }

    #[test]
    fn heart_states_map_health_to_full_half_and_empty_icons() {
        use HeartState::{Empty, Full, Half};
        assert_eq!(heart_states(0.0), [Empty; VITAL_ICON_COUNT]);
        assert_eq!(
            heart_states(1.0),
            [Half, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty]
        );
        assert_eq!(
            heart_states(2.0),
            [Full, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty]
        );
        assert_eq!(
            heart_states(19.0),
            [Full, Full, Full, Full, Full, Full, Full, Full, Full, Half]
        );
        assert_eq!(heart_states(20.0), [Full; VITAL_ICON_COUNT]);
        assert_eq!(heart_states(-4.0), [Empty; VITAL_ICON_COUNT]);
        assert_eq!(heart_states(f32::NAN), [Empty; VITAL_ICON_COUNT]);
        assert_eq!(heart_states(20.5), [Full; VITAL_ICON_COUNT]);
    }

    #[test]
    fn hunger_states_map_hunger_to_full_half_and_empty_icons() {
        use HungerState::{Empty, Full, Half};
        assert_eq!(hunger_states(0.0), [Empty; VITAL_ICON_COUNT]);
        assert_eq!(
            hunger_states(1.0),
            [Half, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty]
        );
        assert_eq!(
            hunger_states(2.0),
            [Full, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty]
        );
        assert_eq!(
            hunger_states(19.0),
            [Full, Full, Full, Full, Full, Full, Full, Full, Full, Half]
        );
        assert_eq!(hunger_states(20.0), [Full; VITAL_ICON_COUNT]);
        assert_eq!(hunger_states(f32::NAN), [Empty; VITAL_ICON_COUNT]);
    }

    #[test]
    fn vitals_are_above_the_hotbar_and_inside_the_screen() {
        let screen = [1280.0, 720.0];
        let hotbar = UiState::default().hotbar_rects(screen);
        let (hearts, hunger) = vitals_layout(screen);
        assert_eq!(hearts.len(), VITAL_ICON_COUNT);
        assert_eq!(hunger.len(), VITAL_ICON_COUNT);
        assert!(hunger[0].y + hunger[0].h <= hotbar[0].y);
        assert!(hearts[0].y + hearts[0].h <= hunger[0].y);
        for rect in hearts.iter().chain(hunger.iter()) {
            assert!(rect.x >= 0.0 && rect.x + rect.w <= screen[0]);
            assert!(rect.y >= 0.0 && rect.y + rect.h <= screen[1]);
        }
    }

    #[test]
    fn damage_flash_timer_triggers_on_health_drop_and_expires() {
        let mut flash = DamageFlash::default();
        assert!(!flash.observe(20.0), "the first sample is only a baseline");
        assert_eq!(flash.remaining(), 0.0);
        assert!(flash.observe(14.0));
        assert_eq!(flash.remaining(), DAMAGE_FLASH_SECONDS);
        assert!((flash.alpha() - 1.0).abs() < 1e-6);
        flash.tick(0.2);
        assert!((flash.remaining() - 0.15).abs() < 1e-6);
        assert!(flash.alpha() < 1.0);
        flash.tick(5.0);
        assert_eq!(flash.remaining(), 0.0);
        assert_eq!(flash.alpha(), 0.0);
        assert!(!flash.observe(14.0), "not dropping again must not flash");
        assert!(flash.observe(13.0));
        flash.reset();
        assert_eq!(flash.remaining(), 0.0);
        // Negative or non-finite frame times do not advance the timer.
        let mut flash = DamageFlash::default();
        flash.observe(20.0);
        flash.observe(10.0);
        flash.tick(f32::NAN);
        flash.tick(-1.0);
        assert_eq!(flash.remaining(), DAMAGE_FLASH_SECONDS);
    }

    #[test]
    fn toast_queue_caps_at_three_and_expires() {
        let mut toasts = ToastQueue::default();
        assert!(toasts.is_empty());
        toasts.push("first");
        toasts.push("second");
        toasts.push_warning("third");
        toasts.push("fourth");
        assert_eq!(toasts.len(), MAX_TOASTS);
        let texts: Vec<&str> = toasts.iter().map(|toast| toast.text.as_str()).collect();
        assert_eq!(texts, ["second", "third", "fourth"]);
        assert_eq!(toasts.iter().next_back().map(|toast| toast.kind), Some(ToastKind::Info));
        toasts.tick(1.0);
        assert_eq!(toasts.len(), MAX_TOASTS);
        toasts.tick(TOAST_SECONDS);
        assert!(toasts.is_empty());
        toasts.push("after");
        toasts.clear();
        assert!(toasts.is_empty());
    }

    #[test]
    fn death_screen_blocks_settings_and_resolves_both_actions() {
        let mut state = UiState::default();
        state.open_death();
        assert_eq!(state.screen, Screen::Death);
        state.open_settings();
        assert_eq!(state.screen, Screen::Death);
        assert_eq!(state.selected_death_action(), Some(DeathAction::Respawn));
        assert_eq!(state.activate_death(), Some(DeathAction::Respawn));
        state.move_death_selection(1);
        assert_eq!(state.selected_death_action(), Some(DeathAction::SaveAndQuit));
        state.move_death_selection(1);
        assert_eq!(state.selected_death_action(), Some(DeathAction::Respawn));
        state.move_death_selection(-1);
        assert_eq!(state.selected_death_action(), Some(DeathAction::SaveAndQuit));
        state.focus_death_buttons();
        assert_eq!(state.selected_death_action(), Some(DeathAction::SaveAndQuit));
        state.clear_death();
        assert_eq!(state.screen, Screen::Playing);
    }

    #[test]
    fn death_heading_uses_the_bitmap_font_and_body_text_uses_the_ui_font() {
        let state = UiState::default();
        let screen = [1280.0, 720.0];
        let mut body = UiFrame::new(screen[0], screen[1]);
        draw_death_screen_custom(
            &mut body,
            &state,
            Some("Test cause"),
            ["Respawn", "Save & quit"],
            DEATH_NOTE,
            "Enter respawn",
            screen,
        );
        let debug = format!("{body:?}");
        assert!(
            debug.contains("texture: Font"),
            "the death heading must use the bitmap atlas"
        );
        assert!(
            debug.contains("texture: UiFont"),
            "buttons/body must use the UI font atlas"
        );

        // Only "You died" with empty labels/cause/note/hint: the bitmap batch
        // remains and the UI-font batch disappears.
        let mut heading = UiFrame::new(screen[0], screen[1]);
        draw_death_screen_custom(&mut heading, &state, None, ["", ""], "", "", screen);
        let debug = format!("{heading:?}");
        assert!(debug.contains("texture: Font"));
        assert!(!debug.contains("texture: UiFont"));
    }

    #[test]
    fn death_buttons_are_centred_and_hit_testable() {
        let screen = [1280.0, 720.0];
        let rects = death_button_rects(screen);
        assert!((rects[0].center().0 - 640.0).abs() < 0.01);
        assert!((rects[1].center().0 - 640.0).abs() < 0.01);
        assert_eq!(rects.len(), DEATH_ACTIONS.len());
        let mut state = UiState::default();
        state.open_death();
        state.cursor = rects[1].center();
        state.update_death_hover(screen);
        assert_eq!(state.death_hovered, Some(1));
        assert_eq!(state.click_death(screen), Some(DeathAction::SaveAndQuit));
        state.cursor = (0.0, 0.0);
        state.update_death_hover(screen);
        assert_eq!(state.death_hovered, None);
    }

    #[test]
    fn death_buttons_have_spacing_and_fit_common_screens() {
        for screen in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            let rects = death_button_rects(screen);
            assert!(rects[1].y - (rects[0].y + rects[0].h) >= ob_render::line_height(2.0) + 4.0);
            assert!(rects[1].y + rects[1].h + 26.0 + ob_render::line_height(1.2) <= screen[1]);
            assert!(rects[0].y > screen[1] / 2.0);
        }
        assert!(DEATH_BUTTON_HEIGHT >= ob_render::line_height(2.0) + 4.0);
    }

    #[test]
    fn death_hint_clears_the_vitals_row_at_960x540() {
        for screen in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            let hint_y = death_hint_y(screen);
            let (hearts, _) = vitals_layout(screen);
            let hint_bottom = hint_y + ob_render::line_height(1.2);
            assert!(
                hint_bottom <= hearts[0].y,
                "death hint overlaps hearts at {screen:?}: {hint_y}..{hint_bottom} vs {}",
                hearts[0].y
            );
            let buttons = death_button_rects(screen);
            assert!(
                hint_y >= buttons[1].y + buttons[1].h + 4.0,
                "death hint crowds the buttons at {screen:?}"
            );
        }
    }

    #[test]
    fn game_mode_row_shows_title_cased_labels() {
        let mut state = UiState::default();
        state.open_settings();
        let info = SettingsInfo::default();
        assert_eq!(state.settings_row_value(SettingsRow::GameMode, &info), "Survival");
        state.settings_adjust(1);
        assert_eq!(state.settings.game_mode, GameMode::Creative);
        assert_eq!(state.settings_row_value(SettingsRow::GameMode, &info), "Creative");
        assert_eq!(SettingsRow::GameMode.label(), "Game mode");
    }

    #[test]
    fn crafting_screen_opens_only_from_gameplay_and_other_modals_keep_priority() {
        let mut state = UiState::default();
        state.toggle_crafting();
        assert_eq!(state.screen, Screen::Crafting);
        state.toggle_crafting();
        assert_eq!(state.screen, Screen::Playing);

        state.open_settings();
        state.toggle_crafting();
        assert_eq!(state.screen, Screen::Settings);
        state.close_settings();
        state.open_death();
        state.toggle_crafting();
        assert_eq!(state.screen, Screen::Death);
        state.clear_death();
        state.open_crafting();
        assert_eq!(state.screen, Screen::Crafting);
        state.close_crafting();
        assert_eq!(state.screen, Screen::Playing);
        assert_eq!(state.crafting.selected, 0);
    }

    #[test]
    fn success_toasts_use_their_own_kind_and_remain_bounded() {
        let mut toasts = ToastQueue::default();
        toasts.push_success("crafted 4 planks");
        assert_eq!(
            toasts.iter().next_back().map(|toast| toast.kind),
            Some(ToastKind::Success)
        );
        for index in 0..5 {
            toasts.push(format!("toast {index}"));
        }
        assert_eq!(toasts.len(), MAX_TOASTS);
        let mut frame = UiFrame::new(1280.0, 720.0);
        draw_toasts(&mut frame, &toasts, [1280.0, 720.0]);
        let (_, quads) = frame.measure();
        assert!(quads >= MAX_TOASTS * 2);
    }

    #[test]
    fn controls_tab_rebinds_rejects_duplicates_and_resets() {
        use ob_core::config::{BindKey, KeyName};

        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Controls);
        let action = match state.settings_activate() {
            Some(SettingsAction::BeginBind(action)) => action,
            other => panic!("expected BeginBind, got {other:?}"),
        };
        assert_eq!(action, Action::MoveForward);
        assert!(state.begin_bind(action));
        assert_eq!(state.binding_action(), Some(Action::MoveForward));
        assert_eq!(
            state.apply_bind(BindKey::Key(KeyName::KeyQ)),
            Ok(()),
            "a free key binds"
        );
        assert_eq!(
            state.settings.keybinds.binding(Action::MoveForward),
            Some(BindKey::Key(KeyName::KeyQ))
        );
        assert_eq!(state.binding_action(), None);
        assert!(!state.binding_message_error);

        // Rebinding Jump to the same key is a conflict and keeps capture open.
        assert!(state.begin_bind(Action::Jump));
        assert_eq!(
            state.apply_bind(BindKey::Key(KeyName::KeyQ)),
            Err(KeybindError::Duplicate {
                action: Action::MoveForward
            })
        );
        assert_eq!(state.binding_action(), Some(Action::Jump));
        assert!(state.binding_message_error);
        state.cancel_bind();
        assert_eq!(state.binding_action(), None);
        assert_eq!(state.binding_message, None);

        // Clearing the focused binding row removes it.
        state.settings_selected_row = 0;
        assert_eq!(state.clear_binding(), Some(Action::MoveForward));
        assert_eq!(state.settings.keybinds.binding(Action::MoveForward), None);
        assert_eq!(
            state.settings_row_value(SettingsRow::Keybind(Action::MoveForward), &SettingsInfo::default()),
            "unbound"
        );

        state.reset_keybinds();
        assert!(state.settings.keybinds.is_default());
        assert!(!state.binding_message_error);
        assert_eq!(
            state.settings.keybinds.binding(Action::MoveForward),
            Some(BindKey::Key(KeyName::KeyW))
        );

        // A text field suppresses capture instead of stealing its keystrokes.
        state.text_input_active = true;
        assert!(!state.begin_bind(Action::Jump));
        assert_eq!(state.binding_action(), None);
        assert!(state.binding_message_error);
    }

    #[test]
    fn host_optional_settings_disable_game_mode_without_hiding_other_rows() {
        let mut state = UiState {
            host_optional: true,
            ..UiState::default()
        };
        state.open_settings();
        assert!(!state.settings_row_enabled(SettingsRow::GameMode));
        assert!(!state.settings_can_adjust(SettingsRow::GameMode, 1));
        assert_eq!(state.settings_activate(), None);
        assert!(state.settings_row_enabled(SettingsRow::RenderDistance));
        assert!(state.settings_row_enabled(SettingsRow::ReducedMotion));
        let info = SettingsInfo::default();
        assert_eq!(
            state.settings_row_value(SettingsRow::GameMode, &info),
            "Survival",
            "the value is still shown, only the row is disabled"
        );
        let mut frame = UiFrame::new(1280.0, 720.0);
        draw_settings_screen(&mut frame, &state, &info, [1280.0, 720.0]);
        let (_, quads) = frame.measure();
        assert!(quads > 0);
    }

    #[test]
    fn reduced_motion_is_a_display_toggle_and_round_trips() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        state.settings_selected_row = 5;
        assert_eq!(state.settings_focused_row(), Some(SettingsRow::ReducedMotion));
        assert_eq!(
            state.settings_activate(),
            Some(SettingsAction::Adjust(SettingsRow::ReducedMotion))
        );
        assert!(state.settings.reduced_motion);
        assert_eq!(
            state.settings_row_value(SettingsRow::ReducedMotion, &SettingsInfo::default()),
            "On"
        );
        assert_eq!(
            state.settings_activate(),
            Some(SettingsAction::Adjust(SettingsRow::ReducedMotion))
        );
        assert!(!state.settings.reduced_motion);
    }

    #[test]
    fn controls_reference_documents_crafting_and_right_click_use() {
        let actions: Vec<&str> = CONTROLS_REFERENCE.iter().map(|binding| binding.action).collect();
        assert!(actions.contains(&"Crafting"));
        assert!(actions.contains(&"Place / eat / use"));
        assert_eq!(CONTROLS_REFERENCE.len(), 14);
        assert_eq!(controls_reference_columns().iter().map(Vec::len).sum::<usize>(), 14);
    }

    fn assert_inside(label: &str, rect: UiRect, screen: [f32; 2]) {
        assert!(
            rect.x >= -0.01
                && rect.y >= -0.01
                && rect.x + rect.w <= screen[0] + 0.01
                && rect.y + rect.h <= screen[1] + 0.01,
            "{label} {rect:?} must fit inside {screen:?}"
        );
    }

    #[test]
    fn gui_scale_matrix_keeps_every_screen_inside_the_logical_target() {
        use crate::{containers, crafting, frontend, inventory, net_ui};
        let mut state = UiState::default();
        state.open_settings();
        for physical in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            for percent in [75_u16, 100, 125, 150] {
                let screen = crate::ui_scale::ui_screen(physical, percent);
                let label = format!("{physical:?} at {percent}% -> {screen:?}");
                state.ui_scale_percent = crate::ui_scale::resolved_ui_scale_percent(percent, physical[1]);

                // Title, world forms and the shared menu panel.
                for rect in frontend::title_button_rects(screen) {
                    assert_inside(&format!("{label} title button"), rect, screen);
                }
                assert_inside(
                    &format!("{label} menu panel"),
                    frontend::menu_panel_rect(screen),
                    screen,
                );
                for rect in frontend::form_button_rects(screen) {
                    assert_inside(&format!("{label} form button"), rect, screen);
                }
                for rect in frontend::multiplayer_field_rects(screen) {
                    assert_inside(&format!("{label} multiplayer field"), rect, screen);
                }

                // Every settings tab, including the ten-row Display tab.
                for tab in SettingsTab::ALL {
                    state.settings_select_tab(tab);
                    let panel = settings_panel_rect(screen);
                    assert_inside(&format!("{label} settings panel {tab:?}"), panel, screen);
                    for rect in settings_tab_rects(screen) {
                        assert_inside(&format!("{label} settings tab {tab:?}"), rect, screen);
                    }
                    let buttons = settings_button_rects(screen);
                    for rect in buttons {
                        assert_inside(&format!("{label} settings button {tab:?}"), rect, screen);
                        assert!(
                            rect.x >= panel.x - 0.01 && rect.x + rect.w <= panel.x + panel.w + 0.01,
                            "{label} settings button {tab:?} must stay in the panel"
                        );
                    }
                    let rows = state.settings_row_rects(screen);
                    for rect in &rows {
                        assert!(rect.x >= panel.x - 0.01 && rect.x + rect.w <= panel.x + panel.w + 0.01);
                        assert!(
                            rect.y >= panel.y - 0.01 && rect.y + rect.h <= panel.y + panel.h + 0.01,
                            "{label} settings row {tab:?} {rect:?} escapes the panel"
                        );
                        assert!(
                            rect.y + rect.h <= buttons[0].y + 0.01,
                            "{label} settings row {tab:?} overlaps the footer buttons"
                        );
                    }
                    for arrows in state.settings_arrow_rects(screen).into_iter().flatten() {
                        assert_inside(&format!("{label} settings arrow {tab:?}"), arrows.0, screen);
                        assert_inside(&format!("{label} settings arrow {tab:?}"), arrows.1, screen);
                    }
                }

                // HUD: crosshair band, hotbar, vitals and armor share the same
                // logical target as the menus.
                for rect in state.hotbar_rects(screen) {
                    assert_inside(&format!("{label} hotbar slot"), rect, screen);
                }
                let (hearts, hunger) = vitals_layout(screen);
                for rect in hearts.iter().chain(hunger.iter()) {
                    assert_inside(&format!("{label} vitals row"), *rect, screen);
                }
                for rect in armor_layout(screen) {
                    assert_inside(&format!("{label} armor row"), rect, screen);
                }

                // Death screen: buttons, hint and vitals must not collide.
                let death_buttons = death_button_rects(screen);
                for rect in death_buttons {
                    assert_inside(&format!("{label} death button"), rect, screen);
                }
                let hint_y = death_hint_y(screen);
                let hint_scale = if screen[1] < 500.0 { 1.0 } else { 1.2 };
                assert!(
                    hint_y + ob_render::line_height(hint_scale) <= hearts[0].y + 0.01,
                    "{label} death hint overlaps the hearts row"
                );
                assert!(
                    death_buttons[1].y + death_buttons[1].h <= hint_y + 0.01,
                    "{label} death hint overlaps the second button"
                );

                // Connect/loading/failure panels in both states.
                for (failed, has_info) in [(false, false), (false, true), (true, false)] {
                    let panel = net_ui::connect_panel_rect_for_state(screen, failed, has_info);
                    assert_inside(&format!("{label} connect panel"), panel, screen);
                    for rect in net_ui::connect_button_rects_for_state(screen, failed, has_info) {
                        assert_inside(&format!("{label} connect button"), rect, screen);
                        assert!(
                            rect.x >= panel.x - 0.01 && rect.x + rect.w <= panel.x + panel.w + 0.01,
                            "{label} connect button must stay in the panel"
                        );
                    }
                }

                // Inventory, crafting and both container screens.
                let survival = inventory::survival_layout(screen);
                assert_inside(&format!("{label} survival inventory"), survival.panel, screen);
                for rect in survival.slots.iter().chain(survival.armor_slots.iter()) {
                    assert_inside(&format!("{label} survival slot"), *rect, screen);
                }
                let creative = inventory::layout(screen, 100, 0);
                assert_inside(&format!("{label} creative inventory"), creative.panel, screen);
                let craft = crafting::crafting_panel_rect(screen);
                assert_inside(&format!("{label} crafting panel"), craft, screen);
                let furnace = containers::furnace_layout(screen);
                assert_inside(&format!("{label} furnace panel"), furnace.panel, screen);
                let chest = containers::chest_layout(screen);
                assert_inside(&format!("{label} chest panel"), chest.panel, screen);
            }
        }
    }

    #[test]
    fn gui_scale_clicks_convert_physical_pointer_to_the_same_logical_rect() {
        use crate::ui_scale::{physical_to_ui_point, ui_to_physical_point};
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        for physical in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            for percent in [75_u16, 100, 125, 150] {
                let screen = crate::ui_scale::ui_screen(physical, percent);
                let factor = crate::ui_scale::ui_scale_factor(percent, physical[1]);
                state.ui_scale_percent = crate::ui_scale::resolved_ui_scale_percent(percent, physical[1]);
                state.settings.render_distance_chunks = 6;
                state.settings_selected_row = 0;
                let (left, _) = state.settings_arrow_rects(screen)[0].expect("range arrows");
                // The window hands us physical pixels; convert exactly as the
                // input loop does before the logical hit test.
                let physical_point = ui_to_physical_point(left.center(), factor);
                state.cursor = physical_to_ui_point(physical_point, factor);
                assert_eq!(
                    state.settings_click(screen),
                    Some(SettingsAction::Adjust(SettingsRow::RenderDistance)),
                    "{physical:?} at {percent}%"
                );
                assert_eq!(state.settings.render_distance_chunks, 5);
            }
        }
    }

    #[test]
    fn gui_scale_row_shows_auto_and_cycles_from_the_resolved_value() {
        let mut state = UiState::default();
        state.open_settings();
        state.settings_select_tab(SettingsTab::Display);
        state.settings_selected_row = 4;
        let info = SettingsInfo {
            ui_scale_percent: 100,
            ..SettingsInfo::default()
        };
        assert_eq!(state.settings_row_value(SettingsRow::UiScale, &info), "Auto (100%)");
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::UiScale));
        assert_eq!(state.settings.ui_scale_percent, 125);
        assert_eq!(state.settings_row_value(SettingsRow::UiScale, &info), "125%");

        // Auto follows the display: at 4K the resolved 150% is the base, so
        // Left selects the next concrete choice below it.
        state.settings.ui_scale_percent = 0;
        state.ui_scale_percent = 150;
        assert_eq!(state.settings_adjust(-1), Some(SettingsRow::UiScale));
        assert_eq!(state.settings.ui_scale_percent, 125);
        state.settings.ui_scale_percent = 0;
        state.ui_scale_percent = 125;
        assert_eq!(state.settings_adjust(1), Some(SettingsRow::UiScale));
        assert_eq!(state.settings.ui_scale_percent, 150);
    }
}

//! Client UI model: F3 debug overlay, HUD layout and the pause/settings menu.
//!
//! This module is deliberately GPU-free: it decides what to show and where,
//! while `ob_render::ui` draws it. Keeping the model separate makes the layout
//! and menu logic unit-testable without a window or a device.

use ob_content::{BlockType, Registry};
use ob_core::config::{
    MAX_FOV_DEGREES, MAX_MOUSE_SENSITIVITY, MAX_RENDER_DISTANCE, MIN_FOV_DEGREES, MIN_MOUSE_SENSITIVITY,
    MIN_RENDER_DISTANCE, Settings,
};

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
/// Height of the settings/pause menu rows.
pub const MENU_ROW_HEIGHT: f32 = 34.0;
/// Width of the central menu panel.
pub const MENU_PANEL_WIDTH: f32 = 420.0;
/// Seconds for which the selected block name stays visible after a change.
pub const BLOCK_NAME_SECONDS: f32 = 2.5;

/// Which screen currently owns input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Normal gameplay with a captured pointer.
    Playing,
    /// Creative inventory with a released pointer.
    Inventory,
    /// Pause menu.
    Pause,
    /// Settings screen reached from the pause menu.
    Settings,
}

/// An interactive row on the pause or settings screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    /// Return to gameplay.
    Resume,
    /// Open the settings screen.
    OpenSettings,
    /// Save the world and exit the process.
    SaveAndQuit,
    /// Return from settings to the pause menu.
    Back,
    /// Decrease render distance.
    RenderDistanceDown,
    /// Increase render distance.
    RenderDistanceUp,
    /// Decrease field of view.
    FovDown,
    /// Increase field of view.
    FovUp,
    /// Decrease mouse sensitivity.
    SensitivityDown,
    /// Increase mouse sensitivity.
    SensitivityUp,
    /// Toggle fullscreen.
    ToggleFullscreen,
    /// Toggle the F3 debug overlay.
    ToggleDebugOverlay,
    /// Toggle HUD visibility.
    ToggleHud,
    /// Select the previous texture style.
    TextureStylePrevious,
    /// Select the next texture style.
    TextureStyleNext,
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
    /// Sections with a cached GPU mesh.
    pub cached_sections: usize,
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
    /// Normalised time of day (0 midnight, 0.5 noon).
    pub time_of_day: f32,
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
                "XYZ {:.3} / {:.3} / {:.3}",
                self.position[0], self.position[1], self.position[2]
            ),
            format!(
                "Chunk {} {} {}   Facing {}",
                self.chunk[0], self.chunk[1], self.chunk[2], self.facing
            ),
            format!(
                "FOV {:.0}   Render distance {} chunks",
                self.fov_degrees, self.render_distance
            ),
            format!("Seed {}", self.seed),
            format!(
                "Chunks {}   Sections drawn {}/{}   Draws {}",
                self.loaded_chunks, self.drawn_sections, self.cached_sections, self.draw_calls
            ),
            format!("Triangles {}", self.triangles),
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
                "Heap {:.1} MiB   Ticks {}",
                self.heap_bytes as f64 / (1024.0 * 1024.0),
                self.ticks
            ),
        ];
        if lines.len() < 12 {
            lines.resize(12, String::new());
        }
        lines
    }

    /// Right column lines: hardware and window information.
    pub fn right_lines(&self) -> Vec<String> {
        vec![
            format!("GPU {}", self.adapter),
            format!("Window {}x{}", self.window[0], self.window[1]),
            "Renderer wgpu (Vulkan)".to_string(),
            "Mesh pipeline: greedy + AO".to_string(),
        ]
    }
}

/// Result of a click on the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Resume gameplay.
    Resume,
    /// Open settings.
    OpenSettings,
    /// Save and exit.
    SaveAndQuit,
    /// Back to the pause menu.
    Back,
    /// Apply one menu item adjustment.
    Adjust(MenuItem),
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
    /// Index of the keyboard-selected menu row.
    pub selected_row: usize,
    /// Index of the hovered menu row, if any.
    pub hovered_row: Option<usize>,
    /// Hotbar slot selected.
    pub hotbar_selected: usize,
    /// Resource ids of the blocks in each hotbar slot.
    pub hotbar: [&'static str; HOTBAR_SLOTS],
    /// Inventory selection, hover and scroll.
    pub inventory: InventoryState,
    /// Remaining seconds to show the selected block name.
    pub block_name_seconds: f32,
    /// Recently broken block name shown briefly near the crosshair.
    pub break_flash: Option<String>,
    /// Remaining seconds to show [`Self::break_flash`].
    pub break_flash_seconds: f32,
    /// Cursor position in UI pixels.
    pub cursor: (f32, f32),
    /// Drag value for a future settings slider.
    pub dragging: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            screen: Screen::Playing,
            debug_overlay: false,
            hud_visible: true,
            settings: Settings::default(),
            selected_row: 0,
            hovered_row: None,
            hotbar_selected: 0,
            hotbar: DEFAULT_HOTBAR,
            inventory: InventoryState::default(),
            block_name_seconds: 0.0,
            break_flash: None,
            break_flash_seconds: 0.0,
            cursor: (0.0, 0.0),
            dragging: false,
        }
    }
}

impl UiState {
    /// Creates state seeded from persisted settings.
    pub fn from_settings(mut settings: Settings) -> Self {
        settings.clamp();
        Self {
            debug_overlay: settings.show_debug_overlay,
            settings,
            ..Self::default()
        }
    }

    /// Advances timers.
    pub fn tick(&mut self, dt_seconds: f32) {
        self.block_name_seconds = (self.block_name_seconds - dt_seconds).max(0.0);
        self.break_flash_seconds = (self.break_flash_seconds - dt_seconds).max(0.0);
        if self.break_flash_seconds <= 0.0 {
            self.break_flash = None;
        }
    }

    /// Toggles the F3 overlay and mirrors it into settings.
    pub fn toggle_debug_overlay(&mut self) {
        self.debug_overlay = !self.debug_overlay;
        self.settings.show_debug_overlay = self.debug_overlay;
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
            Screen::Pause | Screen::Settings => {}
        }
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

    /// Opens the pause screen.
    pub fn open_pause(&mut self) {
        self.screen = Screen::Pause;
        self.selected_row = 0;
        self.hovered_row = None;
    }

    /// Closes menus and returns to gameplay.
    pub fn resume(&mut self) {
        self.screen = Screen::Playing;
    }

    /// Menu rows for the current screen.
    pub fn menu_items(&self) -> Vec<MenuItem> {
        match self.screen {
            Screen::Playing | Screen::Inventory => Vec::new(),
            Screen::Pause => vec![MenuItem::Resume, MenuItem::OpenSettings, MenuItem::SaveAndQuit],
            Screen::Settings => vec![
                MenuItem::RenderDistanceDown,
                MenuItem::RenderDistanceUp,
                MenuItem::FovDown,
                MenuItem::FovUp,
                MenuItem::SensitivityDown,
                MenuItem::SensitivityUp,
                MenuItem::TextureStylePrevious,
                MenuItem::TextureStyleNext,
                MenuItem::ToggleFullscreen,
                MenuItem::ToggleDebugOverlay,
                MenuItem::ToggleHud,
                MenuItem::Back,
            ],
        }
    }

    /// Label text for a settings/pause row, or `None` when it is a button whose
    /// value is rendered separately.
    pub fn row_label(&self, item: MenuItem) -> String {
        match item {
            MenuItem::Resume => "Resume".into(),
            MenuItem::OpenSettings => "Settings".into(),
            MenuItem::SaveAndQuit => "Save and quit".into(),
            MenuItem::Back => "Back".into(),
            MenuItem::RenderDistanceDown => format!("Render distance  {}  (-)", self.settings.render_distance_chunks),
            MenuItem::RenderDistanceUp => "(+)".into(),
            MenuItem::FovDown => format!("Field of view  {:.0} deg  (-)", self.settings.fov_degrees),
            MenuItem::FovUp => "(+)".into(),
            MenuItem::SensitivityDown => format!("Mouse sensitivity  {:.2}x  (-)", self.settings.mouse_sensitivity),
            MenuItem::SensitivityUp => "(+)".into(),
            MenuItem::TextureStylePrevious => {
                format!("Texture style  {}  (<)", self.settings.texture_style.name())
            }
            MenuItem::TextureStyleNext => "(>)".into(),
            MenuItem::ToggleFullscreen => {
                format!("Fullscreen  {}", if self.settings.fullscreen { "on" } else { "off" })
            }
            MenuItem::ToggleDebugOverlay => format!(
                "F3 debug overlay  {}",
                if self.settings.show_debug_overlay { "on" } else { "off" }
            ),
            MenuItem::ToggleHud => format!("HUD  {}", if self.hud_visible { "on" } else { "off" }),
        }
    }

    /// Applies an adjustment, returning whether a setting changed and must be persisted.
    pub fn apply(&mut self, item: MenuItem) -> bool {
        match item {
            MenuItem::Resume => {
                self.resume();
                false
            }
            MenuItem::OpenSettings => {
                self.screen = Screen::Settings;
                self.selected_row = 0;
                false
            }
            MenuItem::SaveAndQuit => false,
            MenuItem::Back => {
                self.screen = Screen::Pause;
                self.selected_row = 0;
                false
            }
            MenuItem::RenderDistanceDown => self.adjust(|s| {
                s.render_distance_chunks = (s.render_distance_chunks - 1).max(MIN_RENDER_DISTANCE);
            }),
            MenuItem::RenderDistanceUp => self.adjust(|s| {
                s.render_distance_chunks = (s.render_distance_chunks + 1).min(MAX_RENDER_DISTANCE);
            }),
            MenuItem::FovDown => self.adjust(|s| {
                s.fov_degrees = (s.fov_degrees - 5.0).max(MIN_FOV_DEGREES);
            }),
            MenuItem::FovUp => self.adjust(|s| {
                s.fov_degrees = (s.fov_degrees + 5.0).min(MAX_FOV_DEGREES);
            }),
            MenuItem::SensitivityDown => self.adjust(|s| {
                s.mouse_sensitivity = (s.mouse_sensitivity - 0.1).max(MIN_MOUSE_SENSITIVITY);
            }),
            MenuItem::SensitivityUp => self.adjust(|s| {
                s.mouse_sensitivity = (s.mouse_sensitivity + 0.1).min(MAX_MOUSE_SENSITIVITY);
            }),
            MenuItem::TextureStylePrevious => {
                self.settings.texture_style = self.settings.texture_style.previous();
                true
            }
            MenuItem::TextureStyleNext => {
                self.settings.texture_style = self.settings.texture_style.next();
                true
            }
            MenuItem::ToggleFullscreen => {
                self.settings.fullscreen = !self.settings.fullscreen;
                true
            }
            MenuItem::ToggleDebugOverlay => {
                self.toggle_debug_overlay();
                true
            }
            MenuItem::ToggleHud => {
                self.hud_visible = !self.hud_visible;
                true
            }
        }
    }

    fn adjust(&mut self, apply: impl FnOnce(&mut Settings)) -> bool {
        let before = self.settings.clone();
        apply(&mut self.settings);
        self.settings.clamp();
        before != self.settings
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

    /// Menu row rects for the current screen and size.
    pub fn menu_rects(&self, screen: [f32; 2]) -> Vec<UiRect> {
        let items = self.menu_items();
        if items.is_empty() {
            return Vec::new();
        }
        let panel_height = items.len() as f32 * MENU_ROW_HEIGHT + 64.0;
        let x = (screen[0] - MENU_PANEL_WIDTH) / 2.0;
        let y = (screen[1] - panel_height) / 2.0 + 48.0;
        (0..items.len())
            .map(|index| {
                UiRect::new(
                    x + 24.0,
                    y + index as f32 * MENU_ROW_HEIGHT,
                    MENU_PANEL_WIDTH - 48.0,
                    MENU_ROW_HEIGHT - 4.0,
                )
            })
            .collect()
    }

    /// Full panel bounds for the current screen.
    pub fn panel_rect(&self, screen: [f32; 2]) -> Option<UiRect> {
        let items = self.menu_items();
        if items.is_empty() {
            return None;
        }
        let height = items.len() as f32 * MENU_ROW_HEIGHT + 64.0;
        Some(UiRect::new(
            (screen[0] - MENU_PANEL_WIDTH) / 2.0,
            (screen[1] - height) / 2.0,
            MENU_PANEL_WIDTH,
            height,
        ))
    }

    /// Updates the hovered row from the cursor.
    pub fn update_hover(&mut self, screen: [f32; 2]) {
        let rects = self.menu_rects(screen);
        self.hovered_row = rects
            .iter()
            .position(|rect| rect.contains(self.cursor.0, self.cursor.1));
    }

    /// Handles a click at the cursor, returning the action to perform.
    pub fn click(&mut self, screen: [f32; 2]) -> Option<MenuAction> {
        self.update_hover(screen);
        let row = self.hovered_row?;
        self.selected_row = row;
        let item = *self.menu_items().get(row)?;
        match item {
            MenuItem::Resume => Some(MenuAction::Resume),
            MenuItem::OpenSettings => Some(MenuAction::OpenSettings),
            MenuItem::SaveAndQuit => Some(MenuAction::SaveAndQuit),
            MenuItem::Back => Some(MenuAction::Back),
            other => {
                self.apply(other);
                Some(MenuAction::Adjust(other))
            }
        }
    }
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

    #[test]
    fn screen_starts_in_playing_with_hud_visible() {
        let state = UiState::default();
        assert_eq!(state.screen, Screen::Playing);
        assert!(state.hud_visible);
        assert!(!state.debug_overlay);
        assert!(state.menu_items().is_empty());
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
    fn inventory_screen_toggles_without_opening_the_pause_menu() {
        let mut state = UiState::default();
        state.toggle_inventory();
        assert_eq!(state.screen, Screen::Inventory);
        assert!(state.menu_items().is_empty());
        assert!(state.panel_rect([1280.0, 720.0]).is_none());
        assert!(state.inventory_layout([1280.0, 720.0], 11).slots.len() == 11);
        state.toggle_inventory();
        assert_eq!(state.screen, Screen::Playing);

        state.open_inventory();
        state.close_inventory();
        assert_eq!(state.screen, Screen::Playing);

        // The pause menu remains reachable only from gameplay.
        state.open_pause();
        state.toggle_inventory();
        assert_eq!(state.screen, Screen::Pause);
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
    fn pause_and_settings_navigation() {
        let mut state = UiState::default();
        state.open_pause();
        assert_eq!(state.screen, Screen::Pause);
        assert_eq!(
            state.menu_items(),
            vec![MenuItem::Resume, MenuItem::OpenSettings, MenuItem::SaveAndQuit]
        );
        state.apply(MenuItem::OpenSettings);
        assert_eq!(state.screen, Screen::Settings);
        assert!(state.menu_items().contains(&MenuItem::RenderDistanceUp));
        state.apply(MenuItem::Back);
        assert_eq!(state.screen, Screen::Pause);
        state.apply(MenuItem::Resume);
        assert_eq!(state.screen, Screen::Playing);
    }

    #[test]
    fn settings_adjustments_are_clamped_and_reported() {
        let mut state = UiState::from_settings(Settings::default());
        assert!(state.apply(MenuItem::RenderDistanceUp));
        assert_eq!(state.settings.render_distance_chunks, 7);
        state.settings.render_distance_chunks = MAX_RENDER_DISTANCE;
        assert!(!state.apply(MenuItem::RenderDistanceUp));
        state.settings.fov_degrees = MIN_FOV_DEGREES;
        assert!(!state.apply(MenuItem::FovDown));
        state.settings.mouse_sensitivity = MIN_MOUSE_SENSITIVITY;
        assert!(!state.apply(MenuItem::SensitivityDown));
        assert!(state.apply(MenuItem::ToggleDebugOverlay));
        assert!(state.settings.show_debug_overlay);
        assert!(state.debug_overlay);
        assert!(state.apply(MenuItem::ToggleHud));
        assert!(!state.hud_visible);
    }

    #[test]
    fn texture_style_rows_cycle_and_persist_signal() {
        let mut state = UiState::default();
        assert_eq!(state.settings.texture_style, TextureStyle::Classic);
        assert!(state.apply(MenuItem::TextureStyleNext));
        assert_eq!(state.settings.texture_style, TextureStyle::Painterly);
        assert!(state.apply(MenuItem::TextureStylePrevious));
        assert_eq!(state.settings.texture_style, TextureStyle::Classic);
        state.settings.texture_style = TextureStyle::Minimal;
        assert!(state.apply(MenuItem::TextureStyleNext));
        assert_eq!(state.settings.texture_style, TextureStyle::Classic);
    }

    #[test]
    fn settings_menu_contains_the_texture_style_rows() {
        let mut state = UiState::default();
        state.open_pause();
        state.apply(MenuItem::OpenSettings);
        let items = state.menu_items();
        assert!(items.contains(&MenuItem::TextureStylePrevious));
        assert!(items.contains(&MenuItem::TextureStyleNext));
        assert_eq!(items.last(), Some(&MenuItem::Back));
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
    fn menu_hit_testing_finds_rows() {
        let mut state = UiState::default();
        state.open_pause();
        let screen = [1280.0, 720.0];
        let rects = state.menu_rects(screen);
        assert_eq!(rects.len(), 3);
        let rect = rects[1];
        state.cursor = rect.center();
        state.update_hover(screen);
        assert_eq!(state.hovered_row, Some(1));
        state.cursor = (1.0, 1.0);
        state.update_hover(screen);
        assert_eq!(state.hovered_row, None);
    }

    #[test]
    fn clicking_settings_rows_applies_and_persists_signal() {
        let mut state = UiState::default();
        state.open_pause();
        state.apply(MenuItem::OpenSettings);
        let screen = [1280.0, 720.0];
        let rects = state.menu_rects(screen);
        // Row 0 is render-distance-down.
        let rect = rects[0];
        state.cursor = rect.center();
        let action = state.click(screen);
        assert_eq!(action, Some(MenuAction::Adjust(MenuItem::RenderDistanceDown)));
        let rect = rects[1];
        let before = state.settings.render_distance_chunks;
        state.cursor = rect.center();
        assert_eq!(
            state.click(screen),
            Some(MenuAction::Adjust(MenuItem::RenderDistanceUp))
        );
        assert_eq!(state.settings.render_distance_chunks, before + 1);
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
            time_of_day: 0.5,
        };
        let left = stats.left_lines();
        assert_eq!(left.len(), 12);
        let joined = left.join("\n");
        for needle in [
            "120 FPS",
            "Chunk",
            "Facing north",
            "Seed 42",
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
    fn pause_panel_is_centred() {
        let mut state = UiState::default();
        state.open_pause();
        let panel = state.panel_rect([1280.0, 720.0]).expect("panel");
        let centre = panel.center();
        assert!((centre.0 - 640.0).abs() < 0.01);
        assert!((centre.1 - 360.0).abs() < 0.01);
    }
}

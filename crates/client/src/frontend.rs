//! Title screen, world selection, new-world and multiplayer front-end.
//!
//! This module is deliberately free of windowing and rendering: every screen's
//! state machine, input validation, on-disk world discovery and layout maths is
//! pure and unit-tested. The windowed and offscreen front-end runtimes in
//! `menu_app` only translate events into these types and draw them with
//! [`ob_render::UiFrame`].
//!
//! The single-player and network runtimes reuse the same controls rebinding and
//! settings machinery (`ui_state`); the front-end only adds the navigation
//! between "no world loaded" and "playing".

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ob_core::config::{MAX_RECENT_SERVERS, RecentServer};
use ob_render::UiFrame;

use crate::net_app::default_player_name;
use crate::ui_state::UiRect;

/// Maximum number of characters accepted in a world name.
pub const MAX_WORLD_NAME_CHARS: usize = 32;
/// Maximum number of digits accepted in the seed field (u64 fits in 20).
pub const MAX_SEED_DIGITS: usize = 20;
/// Default multiplayer endpoint.
pub const DEFAULT_SERVER_ADDRESS: &str = "127.0.0.1:25565";
/// World manifest file that marks a directory as a world.
pub const WORLD_MANIFEST: &str = "world.obs";
/// Rows visible in the world list at a 720p window.
pub const WORLD_LIST_VISIBLE_ROWS: usize = 9;
/// Recent servers visible on the multiplayer screen.
pub const RECENT_SERVER_ROWS: usize = MAX_RECENT_SERVERS;
/// Interval that turns two clicks into a double-click.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Which front-end screen currently owns input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuScreen {
    /// Logo, Continue/New/Load/Multiplayer/Settings/Quit.
    Title,
    /// World name and seed form.
    NewWorld,
    /// Discovered world list.
    LoadWorld,
    /// Server address, player name and recent servers.
    Multiplayer,
    /// The shared settings screen (drawn by `ui_state`).
    Settings,
}

impl MenuScreen {
    /// Human-readable screen name used in logs and the window title.
    pub fn label(self) -> &'static str {
        match self {
            MenuScreen::Title => "Title",
            MenuScreen::NewWorld => "New World",
            MenuScreen::LoadWorld => "Load World",
            MenuScreen::Multiplayer => "Multiplayer",
            MenuScreen::Settings => "Settings",
        }
    }
}

/// One button on the title screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleButton {
    /// Open the last played world.
    Continue,
    /// Open the new-world form.
    NewWorld,
    /// Open the world list.
    LoadWorld,
    /// Open the multiplayer form.
    Multiplayer,
    /// Open settings without a host.
    Settings,
    /// Exit the process.
    Quit,
}

impl TitleButton {
    /// Every button in display order.
    pub const ALL: [TitleButton; 6] = [
        TitleButton::Continue,
        TitleButton::NewWorld,
        TitleButton::LoadWorld,
        TitleButton::Multiplayer,
        TitleButton::Settings,
        TitleButton::Quit,
    ];

    /// Text drawn inside the button.
    pub fn label(self) -> &'static str {
        match self {
            TitleButton::Continue => "Continue",
            TitleButton::NewWorld => "New World",
            TitleButton::LoadWorld => "Load World",
            TitleButton::Multiplayer => "Multiplayer",
            TitleButton::Settings => "Settings",
            TitleButton::Quit => "Quit",
        }
    }
}

/// Keyboard/mouse focus and enablement of the title screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleMenu {
    /// Selected button index into [`TitleButton::ALL`].
    pub selected: usize,
    /// World opened by Continue, or `None` when no valid last world exists.
    pub continue_world: Option<String>,
}

impl TitleMenu {
    /// Creates the menu from the last played world, if it still exists.
    pub fn new(continue_world: Option<String>) -> Self {
        let selected = if continue_world.is_some() { 0 } else { 1 };
        Self {
            selected,
            continue_world,
        }
    }

    /// Whether a button can be activated.
    pub fn enabled(&self, button: TitleButton) -> bool {
        match button {
            TitleButton::Continue => self.continue_world.is_some(),
            _ => true,
        }
    }

    /// Whether the button at `index` is enabled.
    pub fn enabled_at(&self, index: usize) -> bool {
        TitleButton::ALL.get(index).is_some_and(|button| self.enabled(*button))
    }

    /// Moves the selection by `steps`, wrapping and skipping disabled entries.
    pub fn move_selection(&mut self, steps: i32) {
        if steps == 0 || TitleButton::ALL.is_empty() {
            return;
        }
        let count = TitleButton::ALL.len() as i32;
        let mut index = self.selected as i32;
        for _ in 0..count {
            index = (index + steps.signum()).rem_euclid(count);
            if self.enabled_at(index as usize) {
                self.selected = index as usize;
                return;
            }
        }
    }

    /// Currently selected button, enabled or not.
    pub fn selected_button(&self) -> Option<TitleButton> {
        TitleButton::ALL.get(self.selected).copied()
    }

    /// Activates the selection, returning `None` for a disabled entry.
    pub fn activate(&self) -> Option<TitleButton> {
        self.selected_button().filter(|button| self.enabled(*button))
    }

    /// Selects and activates a button directly (mouse click).
    pub fn click(&mut self, button: TitleButton) -> Option<TitleButton> {
        if !self.enabled(button) {
            return None;
        }
        self.selected = TitleButton::ALL.iter().position(|entry| *entry == button).unwrap_or(0);
        Some(button)
    }

    /// Selects the entry under the cursor, if any.
    pub fn update_hover(&mut self, screen: [f32; 2], cursor: (f32, f32)) -> Option<TitleButton> {
        let rects = title_button_rects(screen);
        let index = rects.iter().position(|rect| rect.contains(cursor.0, cursor.1))?;
        if self.enabled_at(index) {
            self.selected = index;
            TitleButton::ALL.get(index).copied()
        } else {
            None
        }
    }
}

impl Default for TitleMenu {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Picks the Continue target: only the persisted last world, and only when its
/// directory still contains a world manifest.
pub fn continue_target(data_dir: &Path, last_world: Option<&str>) -> Option<String> {
    let name = last_world?.trim();
    if name.is_empty() {
        return None;
    }
    world_exists(data_dir, name).then(|| name.to_string())
}

/// Title button rectangles, centred and sized for the target screen.
pub fn title_button_rects(screen: [f32; 2]) -> [UiRect; 6] {
    let width = (screen[0] * 0.32).clamp(240.0, 380.0).min((screen[0] - 32.0).max(1.0));
    let gap = if screen[1] < 420.0 { 4.0 } else { 10.0 };
    let available = (screen[1] * 0.46).max(1.0);
    let height = ((available - gap * 5.0) / 6.0).clamp(22.0, 46.0);
    let total = height * 6.0 + gap * 5.0;
    let start_y = (screen[1] * 0.36).min((screen[1] - total - 24.0).max(0.0));
    let x = (screen[0] - width) / 2.0;
    std::array::from_fn(|index| UiRect::new(x, start_y + index as f32 * (height + gap), width, height))
}

/// Draws the title screen.
pub fn draw_title_screen(frame: &mut UiFrame, menu: &TitleMenu, cursor: (f32, f32), screen: [f32; 2]) {
    draw_backdrop(frame, screen);
    let logo_scale = (screen[0] * 0.8 / (9.0 * 8.0)).clamp(2.0, 9.0);
    let logo_y = (screen[1] * 0.08).max(8.0);
    frame.title_text_centered(
        screen[0] / 2.0,
        logo_y,
        logo_scale,
        [0.99, 0.94, 0.76, 1.0],
        "OPENBLOCK",
    );
    let line = ob_render::line_height(logo_scale);
    frame.title_text_centered(
        screen[0] / 2.0,
        logo_y + line + 6.0,
        if screen[1] < 420.0 { 0.9 } else { 1.5 },
        [0.72, 0.82, 0.96, 1.0],
        "EARLY ACCESS",
    );
    let version = format!(
        "v{}    save format {}    protocol {}    {}/{}",
        ob_core::VERSION,
        ob_core::SAVE_FORMAT_VERSION,
        ob_core::PROTOCOL_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    frame.text_centered(
        screen[0] / 2.0,
        logo_y + line + (if screen[1] < 420.0 { 18.0 } else { 30.0 }),
        0.95,
        [0.72, 0.79, 0.90, 1.0],
        &version,
    );

    let rects = title_button_rects(screen);
    for (index, button) in TitleButton::ALL.iter().enumerate() {
        let rect = rects[index];
        let enabled = menu.enabled(*button);
        let selected = index == menu.selected;
        let hovered = rect.contains(cursor.0, cursor.1);
        draw_menu_button(frame, rect, button.label(), selected, hovered, enabled, false);
    }

    let last_played = menu
        .continue_world
        .as_deref()
        .map_or("No world yet - start a new one".to_string(), |world| {
            format!("Last played: {world}")
        });
    frame.text_centered(
        screen[0] / 2.0,
        (screen[1] - 58.0).max(0.0),
        1.0,
        [0.76, 0.82, 0.91, 1.0],
        &last_played,
    );
    frame.text_centered(
        screen[0] / 2.0,
        (screen[1] - 30.0).max(0.0),
        0.95,
        [0.66, 0.73, 0.83, 1.0],
        "Up/Down select   Enter choose   Mouse click   Esc quit",
    );
}

/// One field of the new-world form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewWorldField {
    /// World directory name.
    Name,
    /// Optional numeric seed.
    Seed,
}

/// New-world form state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorldForm {
    /// World name text.
    pub name: String,
    /// Seed text (empty means 0).
    pub seed: String,
    /// Field currently receiving text input.
    pub field: NewWorldField,
    /// Validation error shown below the fields.
    pub error: Option<String>,
}

impl Default for NewWorldForm {
    fn default() -> Self {
        Self {
            name: "My world".to_string(),
            seed: String::new(),
            field: NewWorldField::Name,
            error: None,
        }
    }
}

/// Validated create-world request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorldRequest {
    /// Filesystem-safe world name.
    pub name: String,
    /// Numeric seed (0 when the field was empty).
    pub seed: u64,
}

impl NewWorldForm {
    /// Switches to the next field (wrapping).
    pub fn next_field(&mut self) {
        self.field = match self.field {
            NewWorldField::Name => NewWorldField::Seed,
            NewWorldField::Seed => NewWorldField::Name,
        };
        self.error = None;
    }

    /// Switches to the previous field (wrapping).
    pub fn previous_field(&mut self) {
        self.field = match self.field {
            NewWorldField::Name => NewWorldField::Seed,
            NewWorldField::Seed => NewWorldField::Name,
        };
        self.error = None;
    }

    /// Current text.
    pub fn text(&self) -> &str {
        match self.field {
            NewWorldField::Name => &self.name,
            NewWorldField::Seed => &self.seed,
        }
    }

    /// Inserts one printable ASCII character into the active field.
    pub fn insert_char(&mut self, character: char) -> bool {
        if !character.is_ascii() || character.is_ascii_control() {
            return false;
        }
        match self.field {
            NewWorldField::Name => {
                if self.name.chars().count() >= MAX_WORLD_NAME_CHARS || !is_world_name_char(character) {
                    return false;
                }
                self.name.push(character);
            }
            NewWorldField::Seed => {
                if !character.is_ascii_digit() || self.seed.len() >= MAX_SEED_DIGITS {
                    return false;
                }
                self.seed.push(character);
            }
        }
        self.error = None;
        true
    }

    /// Removes the last character from the active field.
    pub fn backspace(&mut self) -> bool {
        let text = match self.field {
            NewWorldField::Name => &mut self.name,
            NewWorldField::Seed => &mut self.seed,
        };
        let changed = text.pop().is_some();
        if changed {
            self.error = None;
        }
        changed
    }

    /// Validates and resolves the form into a create request.
    pub fn validate(&self, data_dir: &Path) -> Result<NewWorldRequest, String> {
        let name = validate_world_name(&self.name)?;
        if world_exists(data_dir, &name) {
            return Err(format!(
                "A world named '{name}' already exists. Load it or choose another name."
            ));
        }
        let seed = parse_seed(&self.seed)?;
        Ok(NewWorldRequest { name, seed })
    }
}

/// Draws the new-world form.
pub fn draw_new_world_screen(frame: &mut UiFrame, form: &NewWorldForm, cursor: (f32, f32), screen: [f32; 2]) {
    draw_backdrop(frame, screen);
    let panel = menu_panel_rect(screen);
    draw_panel(frame, panel, "New World");
    let fields = new_world_field_rects(screen);
    let labels = ["World name", "Seed (optional, 0 = default)"];
    let compact = menu_compact(screen);
    for (index, field) in fields.iter().enumerate() {
        let active = (index == 0) == (form.field == NewWorldField::Name);
        let label = labels[index];
        let label_scale = if compact { 1.0 } else { 1.2 };
        let label_gap = if compact { 14.0 } else { 20.0 };
        frame.text(
            field.x,
            field.y - label_gap,
            label_scale,
            [0.74, 0.81, 0.91, 1.0],
            label,
        );
        let text = if index == 0 { &form.name } else { &form.seed };
        let placeholder = if index == 0 { "" } else { "0" };
        draw_text_field(frame, *field, text, placeholder, active, cursor);
    }
    let hint_y = fields[1].y + fields[1].h + if compact { 10.0 } else { 12.0 };
    let hint_lines: [&str; 2] = if compact {
        [
            "Name: letters, digits, spaces, _ - .; max 32 chars.",
            "Worlds are created under <data>/worlds/.",
        ]
    } else {
        [
            "ASCII letters, digits, spaces, underscore, hyphen and dot; max 32 characters.",
            "A new world directory is created under <data>/worlds/.",
        ]
    };
    let max_chars = ((fields[0].w) / ob_render::glyph_advance(1.0)).max(20.0) as usize;
    for (index, line) in hint_lines.iter().enumerate() {
        frame.text(
            fields[0].x,
            hint_y + index as f32 * 14.0,
            1.0,
            [0.60, 0.68, 0.79, 1.0],
            &shorten_text(line, max_chars),
        );
    }
    if let Some(error) = &form.error {
        frame.text(
            fields[0].x,
            hint_y + 32.0,
            1.05,
            [1.0, 0.68, 0.58, 1.0],
            &shorten_text(error, max_chars),
        );
    }
    let [create, back] = form_button_rects(screen);
    for (index, (label, enabled)) in [("Create", true), ("Back", true)].iter().enumerate() {
        let rect = if index == 0 { create } else { back };
        let hovered = rect.contains(cursor.0, cursor.1);
        draw_menu_button(frame, rect, label, hovered, hovered, *enabled, false);
    }
    frame.text_centered(
        screen[0] / 2.0,
        (screen[1] - 30.0).max(0.0),
        0.95,
        [0.66, 0.73, 0.83, 1.0],
        "Tab/Up/Down field   Type to edit   Enter create   Esc back",
    );
}

/// World-list state, including the optional delete confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldList {
    /// Discovered worlds, most recently modified first.
    pub worlds: Vec<WorldSummary>,
    /// Selected row index.
    pub selected: usize,
    /// First visible row.
    pub scroll: usize,
    /// World name awaiting delete confirmation.
    pub confirm_delete: Option<String>,
    /// Last listing/deletion error.
    pub error: Option<String>,
}

impl WorldList {
    /// Creates a list from discovered worlds.
    pub fn new(worlds: Vec<WorldSummary>) -> Self {
        Self {
            worlds,
            selected: 0,
            scroll: 0,
            confirm_delete: None,
            error: None,
        }
    }

    /// Rescans the data directory.
    pub fn refresh(&mut self, data_dir: &Path) {
        self.worlds = discover_worlds(data_dir);
        self.selected = self.selected.min(self.worlds.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.selected);
        self.confirm_delete = None;
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.worlds.is_empty()
    }

    /// Selected world, if any.
    pub fn selected_world(&self) -> Option<&WorldSummary> {
        self.worlds.get(self.selected)
    }

    /// Moves the selection and keeps it visible in `visible_rows`.
    pub fn move_selection(&mut self, steps: i32, visible_rows: usize) {
        if self.worlds.is_empty() || steps == 0 {
            return;
        }
        let count = self.worlds.len() as i32;
        self.selected = (self.selected as i32 + steps).rem_euclid(count) as usize;
        self.ensure_visible(visible_rows);
    }

    /// Ensures the selected row is inside the scroll window.
    pub fn ensure_visible(&mut self, visible_rows: usize) {
        let visible_rows = visible_rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible_rows {
            self.scroll = self.selected + 1 - visible_rows;
        }
        let max_scroll = self.worlds.len().saturating_sub(visible_rows);
        self.scroll = self.scroll.min(max_scroll);
    }

    /// Selects the row under the cursor.
    pub fn select_at(&mut self, screen: [f32; 2], cursor: (f32, f32)) -> Option<usize> {
        let visible = visible_row_rects(screen, self.visible_rows(screen));
        let row = visible
            .iter()
            .enumerate()
            .find(|(_, rect)| rect.contains(cursor.0, cursor.1))
            .map(|(index, _)| self.scroll + index)?;
        if row < self.worlds.len() {
            self.selected = row;
            Some(row)
        } else {
            None
        }
    }

    /// Number of rows that fit on `screen`.
    pub fn visible_rows(&self, screen: [f32; 2]) -> usize {
        world_row_capacity(screen)
    }

    /// Starts the delete confirmation for the selected world.
    pub fn request_delete(&mut self) -> Option<String> {
        let name = self.selected_world()?.name.clone();
        self.confirm_delete = Some(name.clone());
        self.error = None;
        Some(name)
    }

    /// Cancels the delete confirmation.
    pub fn cancel_delete(&mut self) {
        self.confirm_delete = None;
    }

    /// Deletes the confirmed world directory and refreshes the list.
    pub fn confirm_delete(&mut self, data_dir: &Path) -> Result<(), String> {
        let Some(name) = self.confirm_delete.take() else {
            return Err("No world is awaiting deletion.".to_string());
        };
        delete_world(data_dir, &name)?;
        self.refresh(data_dir);
        Ok(())
    }
}

/// Draws the world list.
pub fn draw_world_list_screen(frame: &mut UiFrame, list: &WorldList, cursor: (f32, f32), screen: [f32; 2]) {
    draw_backdrop(frame, screen);
    let panel = menu_panel_rect(screen);
    draw_panel(frame, panel, "Load World");
    let visible_rows = list.visible_rows(screen);
    let rows = visible_row_rects(screen, visible_rows);
    if list.is_empty() {
        frame.text_centered(
            panel.x + panel.w / 2.0,
            panel.y + 150.0,
            1.5,
            [0.78, 0.84, 0.92, 1.0],
            "No worlds yet",
        );
        frame.text_centered(
            panel.x + panel.w / 2.0,
            panel.y + 178.0,
            1.1,
            [0.60, 0.68, 0.79, 1.0],
            "Create one from the title screen to start playing.",
        );
    }
    for (index, rect) in rows.iter().enumerate() {
        let world_index = list.scroll + index;
        let Some(world) = list.worlds.get(world_index) else {
            break;
        };
        let selected = world_index == list.selected;
        let hovered = rect.contains(cursor.0, cursor.1);
        draw_world_row(frame, *rect, world, selected, hovered);
    }
    if list.worlds.len() > visible_rows {
        let first = list.scroll + 1;
        let last = (list.scroll + visible_rows).min(list.worlds.len());
        frame.text(
            panel.x + panel.w - 110.0,
            panel.y + 64.0,
            0.95,
            [0.55, 0.63, 0.75, 1.0],
            &format!("{first}-{last} of {}", list.worlds.len()),
        );
    }
    if let Some(error) = &list.error {
        frame.text(
            panel.x + 30.0,
            panel.y + 76.0,
            1.05,
            [1.0, 0.66, 0.56, 1.0],
            &shorten_text(error, 96),
        );
    }
    let [play, delete, back] = world_list_button_rects(screen);
    let play_enabled = list.selected_world().is_some();
    draw_menu_button(
        frame,
        play,
        "Play",
        play_enabled && play.contains(cursor.0, cursor.1),
        play.contains(cursor.0, cursor.1),
        play_enabled,
        false,
    );
    draw_menu_button(
        frame,
        delete,
        "Delete",
        play_enabled && delete.contains(cursor.0, cursor.1),
        delete.contains(cursor.0, cursor.1),
        play_enabled,
        true,
    );
    draw_menu_button(
        frame,
        back,
        "Back",
        back.contains(cursor.0, cursor.1),
        back.contains(cursor.0, cursor.1),
        true,
        false,
    );
    frame.text_centered(
        screen[0] / 2.0,
        (screen[1] - 30.0).max(0.0),
        0.95,
        [0.66, 0.73, 0.83, 1.0],
        "Up/Down select   Enter or double-click play   Delete remove   Esc back",
    );
    if let Some(name) = &list.confirm_delete {
        draw_delete_confirmation(frame, name, cursor, screen);
    }
}

/// Confirmation dialog button rectangles: `[delete, cancel]`.
pub fn delete_confirm_button_rects(screen: [f32; 2]) -> [UiRect; 2] {
    let panel = menu_panel_rect(screen);
    let width = (panel.w * 0.3).clamp(130.0, 190.0);
    let height = 40.0;
    let gap = 18.0;
    let total = width * 2.0 + gap;
    let x = panel.x + (panel.w - total) / 2.0;
    let y = panel.y + panel.h / 2.0 + 44.0;
    [
        UiRect::new(x, y, width, height),
        UiRect::new(x + width + gap, y, width, height),
    ]
}

/// Draws the delete confirmation over the world list.
pub fn draw_delete_confirmation(frame: &mut UiFrame, name: &str, cursor: (f32, f32), screen: [f32; 2]) {
    frame.rect(0.0, 0.0, screen[0], screen[1], [0.0, 0.0, 0.0, 0.62]);
    let panel = menu_panel_rect(screen);
    let box_width = (panel.w * 0.66).min(520.0);
    let box_height = 190.0;
    let dialog = UiRect::new(
        (screen[0] - box_width) / 2.0,
        (screen[1] - box_height) / 2.0,
        box_width,
        box_height,
    );
    frame.rect(
        dialog.x + 5.0,
        dialog.y + 6.0,
        dialog.w,
        dialog.h,
        [0.0, 0.0, 0.0, 0.35],
    );
    frame.rect(dialog.x, dialog.y, dialog.w, dialog.h, [0.10, 0.09, 0.11, 0.99]);
    frame.outline(dialog.x, dialog.y, dialog.w, dialog.h, 2.0, [0.86, 0.48, 0.40, 1.0]);
    frame.title_text_centered(
        dialog.x + dialog.w / 2.0,
        dialog.y + 22.0,
        2.0,
        [1.0, 0.92, 0.88, 1.0],
        "Delete world?",
    );
    frame.text_centered(
        dialog.x + dialog.w / 2.0,
        dialog.y + 64.0,
        1.35,
        [0.92, 0.95, 0.99, 1.0],
        &format!("'{name}'"),
    );
    frame.text_centered(
        dialog.x + dialog.w / 2.0,
        dialog.y + 88.0,
        1.05,
        [0.80, 0.72, 0.70, 1.0],
        "The world directory and everything in it will be removed.",
    );
    let [delete, cancel] = delete_confirm_button_rects(screen);
    draw_menu_button(
        frame,
        delete,
        "Delete",
        delete.contains(cursor.0, cursor.1),
        delete.contains(cursor.0, cursor.1),
        true,
        true,
    );
    draw_menu_button(
        frame,
        cancel,
        "Cancel",
        cancel.contains(cursor.0, cursor.1),
        cancel.contains(cursor.0, cursor.1),
        true,
        false,
    );
    frame.text_centered(
        dialog.x + dialog.w / 2.0,
        dialog.y + dialog.h - 28.0,
        0.95,
        [0.62, 0.68, 0.76, 1.0],
        "Enter/Y delete   Esc/N cancel",
    );
}

/// One text field on the multiplayer form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiplayerField {
    /// Server endpoint.
    Address,
    /// Requested player name.
    Name,
    /// Recent-server list.
    Recents,
}

/// Multiplayer form state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiplayerForm {
    /// Server address text.
    pub address: String,
    /// Player name text.
    pub name: String,
    /// Focused field or list.
    pub field: MultiplayerField,
    /// Selected recent server.
    pub recent_selected: usize,
    /// Validation error shown below the fields.
    pub error: Option<String>,
}

impl Default for MultiplayerForm {
    fn default() -> Self {
        Self {
            address: DEFAULT_SERVER_ADDRESS.to_string(),
            name: default_player_name(),
            field: MultiplayerField::Address,
            recent_selected: 0,
            error: None,
        }
    }
}

/// Validated multiplayer connection request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectRequest {
    /// Canonical `host:port` endpoint.
    pub address: String,
    /// Validated player name.
    pub name: String,
}

impl MultiplayerForm {
    /// Creates a form pre-filled with the default endpoint and name.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advances focus: Address -> Name -> Recents -> Address.
    pub fn next_field(&mut self, recent_count: usize) {
        self.field = match self.field {
            MultiplayerField::Address => MultiplayerField::Name,
            MultiplayerField::Name if recent_count > 0 => MultiplayerField::Recents,
            MultiplayerField::Name => MultiplayerField::Address,
            MultiplayerField::Recents => MultiplayerField::Address,
        };
        self.error = None;
    }

    /// Moves focus backwards.
    pub fn previous_field(&mut self, recent_count: usize) {
        self.field = match self.field {
            MultiplayerField::Address if recent_count > 0 => MultiplayerField::Recents,
            MultiplayerField::Address => MultiplayerField::Name,
            MultiplayerField::Name => MultiplayerField::Address,
            MultiplayerField::Recents => MultiplayerField::Name,
        };
        self.error = None;
    }

    /// Text of the focused editable field.
    pub fn text(&self) -> &str {
        match self.field {
            MultiplayerField::Address => &self.address,
            MultiplayerField::Name => &self.name,
            MultiplayerField::Recents => "",
        }
    }

    /// Inserts one printable ASCII character into the focused text field.
    pub fn insert_char(&mut self, character: char) -> bool {
        if !character.is_ascii() || character.is_ascii_control() {
            return false;
        }
        let field = match self.field {
            MultiplayerField::Address => &mut self.address,
            MultiplayerField::Name => &mut self.name,
            MultiplayerField::Recents => return false,
        };
        if field.chars().count() >= 255 {
            return false;
        }
        field.push(character);
        self.error = None;
        true
    }

    /// Removes the last character from the focused text field.
    pub fn backspace(&mut self) -> bool {
        let field = match self.field {
            MultiplayerField::Address => &mut self.address,
            MultiplayerField::Name => &mut self.name,
            MultiplayerField::Recents => return false,
        };
        let changed = field.pop().is_some();
        if changed {
            self.error = None;
        }
        changed
    }

    /// Moves the recent-server selection.
    pub fn move_recent(&mut self, steps: i32, recent_count: usize) {
        if recent_count == 0 {
            self.recent_selected = 0;
            return;
        }
        let count = recent_count.min(MAX_RECENT_SERVERS) as i32;
        self.recent_selected = (self.recent_selected as i32 + steps).rem_euclid(count) as usize;
    }

    /// Copies a remembered server into the form.
    pub fn select_recent(&mut self, recent: &RecentServer) {
        self.address = recent.address.clone();
        if !recent.name.is_empty() {
            self.name = recent.name.clone();
        }
        self.field = MultiplayerField::Address;
        self.error = None;
    }

    /// Validates the form into a connection request.
    pub fn validate(&self) -> Result<ConnectRequest, String> {
        let address = validate_server_address(&self.address)?;
        let name = validate_player_name(&self.name)?;
        Ok(ConnectRequest { address, name })
    }
}

/// Draws the multiplayer form.
pub fn draw_multiplayer_screen(
    frame: &mut UiFrame,
    form: &MultiplayerForm,
    recents: &[RecentServer],
    cursor: (f32, f32),
    screen: [f32; 2],
) {
    draw_backdrop(frame, screen);
    let panel = menu_panel_rect(screen);
    draw_panel(frame, panel, "Multiplayer");
    let fields = multiplayer_field_rects(screen);
    let compact = menu_compact(screen);
    let labels = ["Server address (host:port)", "Player name"];
    for (index, field) in fields.iter().enumerate() {
        let active = (index == 0 && form.field == MultiplayerField::Address)
            || (index == 1 && form.field == MultiplayerField::Name);
        frame.text(
            field.x,
            field.y - if compact { 14.0 } else { 20.0 },
            if compact { 1.0 } else { 1.2 },
            [0.74, 0.81, 0.91, 1.0],
            labels[index],
        );
        let (text, placeholder) = if index == 0 {
            (&form.address, DEFAULT_SERVER_ADDRESS)
        } else {
            (&form.name, "playerNNNN")
        };
        draw_text_field(frame, *field, text, placeholder, active, cursor);
    }
    if !compact {
        frame.text(
            fields[0].x,
            fields[1].y + fields[1].h + 10.0,
            1.0,
            [0.60, 0.68, 0.79, 1.0],
            "The server must run the same protocol version. Survival is server-authoritative.",
        );
    }
    let list_label_y = fields[1].y + fields[1].h + if compact { 16.0 } else { 36.0 };
    frame.text(
        fields[0].x,
        list_label_y,
        if compact { 1.05 } else { 1.2 },
        [0.86, 0.91, 0.99, 1.0],
        if compact {
            "Recent servers (protocol v3 required)"
        } else {
            "Recent servers"
        },
    );
    let rows = recent_server_rects(screen);
    if recents.is_empty() {
        frame.text(
            fields[0].x,
            list_label_y + 22.0,
            1.0,
            [0.58, 0.65, 0.75, 1.0],
            "No servers yet. Connect once and this list remembers the last five.",
        );
    }
    for (index, rect) in rows.iter().enumerate() {
        let Some(recent) = recents.get(index) else {
            break;
        };
        let selected = form.field == MultiplayerField::Recents && index == form.recent_selected;
        let hovered = rect.contains(cursor.0, cursor.1);
        let background = if selected {
            [0.16, 0.22, 0.31, 0.98]
        } else if hovered {
            [0.12, 0.16, 0.23, 0.95]
        } else {
            [0.07, 0.09, 0.13, 0.85]
        };
        frame.rect(rect.x, rect.y, rect.w, rect.h, background);
        if selected {
            frame.outline(rect.x, rect.y, rect.w, rect.h, 1.0, [0.95, 0.79, 0.35, 0.95]);
        }
        let row_scale = if rect.h < 24.0 { 1.0 } else { 1.15 };
        let text_y = rect.y + (rect.h - ob_render::line_height(row_scale)) / 2.0;
        let name_text = if recent.name.is_empty() {
            String::new()
        } else {
            format!("as {}", shorten_text(&recent.name, 24))
        };
        let name_width = if name_text.is_empty() {
            0.0
        } else {
            frame.text_width(0.95, &name_text) + 12.0
        };
        let address_chars = (((rect.w - 24.0 - name_width) / ob_render::glyph_advance(row_scale)).max(8.0)) as usize;
        frame.text(
            rect.x + 12.0,
            text_y,
            row_scale,
            [0.88, 0.93, 0.99, 1.0],
            &shorten_text(&recent.address, address_chars),
        );
        if !name_text.is_empty() {
            let width = frame.text_width(0.95, &name_text);
            let name_y = rect.y + (rect.h - ob_render::line_height(0.95)) / 2.0;
            frame.text(
                rect.x + rect.w - width - 12.0,
                name_y,
                0.95,
                [0.62, 0.70, 0.80, 1.0],
                &name_text,
            );
        }
    }
    if let Some(error) = &form.error {
        let y = multiplayer_error_y(screen);
        frame.text(fields[0].x, y, 1.15, [1.0, 0.68, 0.58, 1.0], &shorten_text(error, 96));
    }
    let [connect, back] = form_button_rects(screen);
    draw_menu_button(
        frame,
        connect,
        "Connect",
        connect.contains(cursor.0, cursor.1),
        connect.contains(cursor.0, cursor.1),
        true,
        false,
    );
    draw_menu_button(
        frame,
        back,
        "Back",
        back.contains(cursor.0, cursor.1),
        back.contains(cursor.0, cursor.1),
        true,
        false,
    );
    frame.text_centered(
        screen[0] / 2.0,
        (screen[1] - 30.0).max(0.0),
        0.95,
        [0.66, 0.73, 0.83, 1.0],
        "Tab/Up/Down field   Type to edit   Enter connect   Esc back",
    );
}

/// Whether the front-end uses its compact vertical layout.
///
/// 480 logical pixels is the 1280x720 baseline. Below it (for example
/// 960x540 at 150% GUI scale, which is 640x360 logical) fields, labels and
/// buttons are tightened so the footer hint stays outside the panel.
fn menu_compact(screen: [f32; 2]) -> bool {
    screen[1] < 480.0
}

/// Shared menu panel bounds.
pub fn menu_panel_rect(screen: [f32; 2]) -> UiRect {
    let width = (screen[0] - 48.0).clamp(1.0, 760.0);
    // Keep the footer band and hint text clear of the panel. At 960x540 the
    // old `screen - 48` panel reached into the footer and the hint clipped.
    let reserved = if screen[1] >= 480.0 {
        96.0
    } else if screen[1] >= 320.0 {
        72.0
    } else {
        48.0
    };
    let height = (screen[1] - reserved).clamp(1.0, 560.0);
    UiRect::new((screen[0] - width) / 2.0, (screen[1] - height) / 2.0, width, height)
}

/// Bounds of the two text fields on the new-world form.
pub fn new_world_field_rects(screen: [f32; 2]) -> [UiRect; 2] {
    let panel = menu_panel_rect(screen);
    let compact = menu_compact(screen);
    let width = panel.w - 72.0;
    let x = panel.x + 36.0;
    let top = panel.y + if compact { 60.0 } else { (panel.h * 0.22).max(72.0) };
    let gap = if compact {
        (panel.h * 0.16).clamp(52.0, 58.0)
    } else {
        (panel.h * 0.14).clamp(68.0, 76.0)
    };
    let height = if compact { 32.0 } else { 40.0 };
    [
        UiRect::new(x, top, width, height),
        UiRect::new(x, top + gap, width, height),
    ]
}

/// Bounds of the two text fields on the multiplayer form.
pub fn multiplayer_field_rects(screen: [f32; 2]) -> [UiRect; 2] {
    let panel = menu_panel_rect(screen);
    let compact = menu_compact(screen);
    let width = panel.w - 72.0;
    let x = panel.x + 36.0;
    let top = panel.y + if compact { 60.0 } else { (panel.h * 0.20).max(72.0) };
    let gap = if compact {
        (panel.h * 0.14).clamp(52.0, 58.0)
    } else {
        (panel.h * 0.13).clamp(68.0, 74.0)
    };
    let height = if compact { 32.0 } else { 40.0 };
    [
        UiRect::new(x, top, width, height),
        UiRect::new(x, top + gap, width, height),
    ]
}

/// Bounds of the recent-server rows on the multiplayer form.
pub fn recent_server_rects(screen: [f32; 2]) -> Vec<UiRect> {
    let panel = menu_panel_rect(screen);
    let compact = menu_compact(screen);
    let x = panel.x + 36.0;
    let width = panel.w - 72.0;
    let fields = multiplayer_field_rects(screen);
    let buttons = form_button_rects(screen);
    let (top, pitch, height) = if compact {
        (fields[1].y + fields[1].h + 28.0, 26.0, 22.0)
    } else {
        (
            (panel.y + panel.h * 0.54).max(fields[1].y + fields[1].h + 44.0),
            30.0,
            26.0,
        )
    };
    let available = (buttons[0].y - 12.0 - top).max(0.0);
    let capacity = ((available / pitch).floor() as usize).min(RECENT_SERVER_ROWS);
    (0..capacity)
        .map(|index| UiRect::new(x, top + index as f32 * pitch, width, height))
        .collect()
}

/// Bounds of the Create and Back buttons on the new-world form.
pub fn form_button_rects(screen: [f32; 2]) -> [UiRect; 2] {
    let panel = menu_panel_rect(screen);
    let width = (panel.w * 0.24).clamp(120.0, 170.0);
    let height = (panel.h * 0.09).clamp(30.0, 42.0);
    let gap = 20.0;
    let total = width * 2.0 + gap;
    let x = panel.x + (panel.w - total) / 2.0;
    let y = panel.y + panel.h - height - if menu_compact(screen) { 18.0 } else { 46.0 };
    [
        UiRect::new(x, y, width, height),
        UiRect::new(x + width + gap, y, width, height),
    ]
}

/// Y of the multiplayer validation error line.
///
/// It sits a fixed distance above the Connect button so a long error or a full
/// recent-server list cannot draw over the button at 960x540.
pub fn multiplayer_error_y(screen: [f32; 2]) -> f32 {
    let buttons = form_button_rects(screen);
    let fields = multiplayer_field_rects(screen);
    (buttons[0].y - 22.0).max(fields[1].y + fields[1].h + 40.0)
}

/// Bounds of the Play, Delete and Back buttons on the world list.
pub fn world_list_button_rects(screen: [f32; 2]) -> [UiRect; 3] {
    let panel = menu_panel_rect(screen);
    let width = (panel.w * 0.18).clamp(100.0, 150.0);
    let height = (panel.h * 0.09).clamp(30.0, 42.0);
    let gap = 14.0;
    let total = width * 3.0 + gap * 2.0;
    let x = panel.x + (panel.w - total) / 2.0;
    let y = panel.y + panel.h - height - if menu_compact(screen) { 18.0 } else { 42.0 };
    [
        UiRect::new(x, y, width, height),
        UiRect::new(x + width + gap, y, width, height),
        UiRect::new(x + (width + gap) * 2.0, y, width, height),
    ]
}

/// Row capacity of the world list for a screen size.
pub fn world_row_capacity(screen: [f32; 2]) -> usize {
    let panel = menu_panel_rect(screen);
    let buttons = world_list_button_rects(screen);
    let top = panel.y + 104.0;
    let bottom = buttons[0].y - 10.0;
    let row_step = 40.0;
    (((bottom - top) / row_step).floor() as usize).clamp(1, WORLD_LIST_VISIBLE_ROWS)
}

/// Row rectangles of the visible world list.
pub fn visible_row_rects(screen: [f32; 2], visible_rows: usize) -> Vec<UiRect> {
    let panel = menu_panel_rect(screen);
    let width = panel.w - 64.0;
    let x = panel.x + 32.0;
    let top = panel.y + 104.0;
    (0..visible_rows)
        .map(|index| UiRect::new(x, top + index as f32 * 40.0, width, 36.0))
        .collect()
}

/// Formats a byte count for the world list.
pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

/// Formats a modification time relative to `now` for the world list.
pub fn format_modified(modified: SystemTime, now: SystemTime) -> String {
    let Ok(elapsed) = now.duration_since(modified) else {
        return "just now".to_string();
    };
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3_600 {
        format!("{} min ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} h ago", seconds / 3_600)
    } else if seconds < 86_400 * 30 {
        format!("{} d ago", seconds / 86_400)
    } else if seconds < 86_400 * 365 {
        format!("{} mo ago", seconds / (86_400 * 30))
    } else {
        format!("{} y ago", seconds / (86_400 * 365))
    }
}

/// One discovered world directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldSummary {
    /// Directory name (the world name).
    pub name: String,
    /// Absolute or caller-relative world directory.
    pub path: PathBuf,
    /// Size of `world.obs` in bytes.
    pub size_bytes: u64,
    /// Last modification time of the world manifest or directory.
    pub modified: Option<SystemTime>,
}

impl WorldSummary {
    /// Size label for drawing.
    pub fn size_label(&self) -> String {
        format_size(self.size_bytes)
    }

    /// Modification label for drawing.
    pub fn modified_label(&self, now: SystemTime) -> String {
        self.modified
            .map_or_else(|| "unknown".to_string(), |modified| format_modified(modified, now))
    }
}

/// Lists worlds under `<data_dir>/worlds/<name>` that contain a `world.obs`.
pub fn discover_worlds(data_dir: &Path) -> Vec<WorldSummary> {
    let worlds_dir = data_dir.join("worlds");
    let Ok(entries) = std::fs::read_dir(&worlds_dir) else {
        return Vec::new();
    };
    let mut worlds = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        let manifest = path.join(WORLD_MANIFEST);
        let Ok(manifest_meta) = std::fs::metadata(&manifest) else {
            continue;
        };
        if !manifest_meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.is_empty() {
            continue;
        }
        let modified = manifest_meta
            .modified()
            .or_else(|_| std::fs::metadata(&path).and_then(|meta| meta.modified()))
            .ok();
        worlds.push(WorldSummary {
            name,
            path,
            size_bytes: manifest_meta.len(),
            modified,
        });
    }
    sort_worlds(&mut worlds);
    worlds
}

/// Sorts worlds with the most recently modified first, then by name.
pub fn sort_worlds(worlds: &mut [WorldSummary]) {
    worlds.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.name.to_ascii_lowercase().cmp(&right.name.to_ascii_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// Validates a world name and returns it trimmed.
pub fn validate_world_name(input: &str) -> Result<String, String> {
    let name = input.trim();
    if name.is_empty() {
        return Err("Enter a world name.".to_string());
    }
    if name.chars().count() > MAX_WORLD_NAME_CHARS {
        return Err(format!("World names are at most {MAX_WORLD_NAME_CHARS} characters."));
    }
    if !name.is_ascii() {
        return Err("World names must be plain ASCII.".to_string());
    }
    if !name.chars().all(is_world_name_char) {
        return Err("Use only letters, digits, spaces, '_', '-' and '.'.".to_string());
    }
    if name == "." || name == ".." || name.ends_with('.') {
        return Err("That name is not safe as a directory name.".to_string());
    }
    if is_reserved_device_name(name) {
        return Err(format!("'{name}' is reserved on Windows; choose another name."));
    }
    Ok(name.to_string())
}

/// Parses the optional seed field: empty means 0.
pub fn parse_seed(input: &str) -> Result<u64, String> {
    let seed = input.trim();
    if seed.is_empty() {
        return Ok(0);
    }
    seed.parse::<u64>()
        .map_err(|_| "Seed must be a whole number between 0 and 18446744073709551615.".to_string())
}

/// Validates a `host:port` endpoint, accepting bracketed IPv6 literals.
pub fn validate_server_address(input: &str) -> Result<String, String> {
    let address = input.trim();
    if address.is_empty() {
        return Err(format!("Enter a server address like {DEFAULT_SERVER_ADDRESS}."));
    }
    if address.chars().count() > 255 {
        return Err("Server addresses are at most 255 characters.".to_string());
    }
    if address
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err("Server addresses cannot contain spaces or control characters.".to_string());
    }
    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return Err("Close the IPv6 address with ']'.".to_string());
        };
        let Some(port) = tail.strip_prefix(':') else {
            return Err("Add a port, for example [::1]:25565.".to_string());
        };
        (host, port)
    } else {
        let Some((host, port)) = address.rsplit_once(':') else {
            return Err(format!("Add a port, for example {DEFAULT_SERVER_ADDRESS}."));
        };
        if host.contains(':') {
            return Err("Wrap IPv6 addresses in brackets, for example [::1]:25565.".to_string());
        }
        (host, port)
    };
    if host.is_empty() {
        return Err("The server host is empty.".to_string());
    }
    if !host
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':'))
    {
        return Err("The host may only contain ASCII letters, digits, '.', '-', '_' and ':'.".to_string());
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| "The port must be a number between 1 and 65535.".to_string())?;
    if port == 0 {
        return Err("The port must be between 1 and 65535.".to_string());
    }
    Ok(address.to_string())
}

/// Validates a multiplayer player name with the protocol's rule.
pub fn validate_player_name(input: &str) -> Result<String, String> {
    let name = input.trim();
    ob_protocol::validate_player_name(name)
        .map_err(|_| "Use 1-32 ASCII letters, digits, '_', '-' or '.' for the player name.".to_string())?;
    Ok(name.to_string())
}

/// Deletes one world directory after proving it is a direct child of the
/// worlds directory and contains a world manifest.
pub fn delete_world(data_dir: &Path, name: &str) -> Result<(), String> {
    let name = validate_world_name(name)?;
    let worlds_dir = data_dir.join("worlds");
    let target = worlds_dir.join(&name);
    if !target.join(WORLD_MANIFEST).is_file() {
        return Err(format!("'{name}' is not a world directory."));
    }
    let root = worlds_dir
        .canonicalize()
        .map_err(|error| format!("Could not resolve the worlds directory: {error}"))?;
    let canonical = target
        .canonicalize()
        .map_err(|error| format!("Could not resolve the world directory: {error}"))?;
    if canonical.parent() != Some(root.as_path()) {
        return Err("Refusing to delete a directory outside the worlds folder.".to_string());
    }
    std::fs::remove_dir_all(&canonical).map_err(|error| format!("Could not delete the world: {error}"))
}

/// Whether `name` is a world with a manifest under `data_dir`.
pub fn world_exists(data_dir: &Path, name: &str) -> bool {
    data_dir.join("worlds").join(name).join(WORLD_MANIFEST).is_file()
}

fn is_world_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.')
}

fn is_reserved_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(
        base.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn shorten_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    short.push_str("...");
    short
}

fn draw_backdrop(frame: &mut UiFrame, screen: [f32; 2]) {
    // The renderer paints the sky; a dark translucent wash keeps the menu text
    // readable over any time of day and gives every screen a consistent look.
    frame.rect(0.0, 0.0, screen[0], screen[1], [0.02, 0.035, 0.065, 0.62]);
    let band_height = (screen[1] / 12.0).max(1.0);
    for index in 0..12 {
        let t = index as f32 / 12.0;
        let alpha = 0.03 + t * 0.10;
        frame.rect(
            0.0,
            screen[1] - band_height * (index + 1) as f32,
            screen[0],
            band_height,
            [0.04 + t * 0.02, 0.07 + t * 0.04, 0.12 + t * 0.05, alpha],
        );
    }
    frame.rect(0.0, screen[1] - 44.0, screen[0], 44.0, [0.02, 0.03, 0.05, 0.72]);
    frame.rect(0.0, 0.0, screen[0], 2.0, [0.55, 0.45, 0.24, 0.55]);
}

fn draw_panel(frame: &mut UiFrame, panel: UiRect, title: &str) {
    frame.rect(panel.x + 6.0, panel.y + 8.0, panel.w, panel.h, [0.0, 0.0, 0.0, 0.38]);
    frame.rect(panel.x, panel.y, panel.w, panel.h, [0.055, 0.068, 0.092, 0.975]);
    frame.outline(panel.x, panel.y, panel.w, panel.h, 2.0, [0.43, 0.49, 0.58, 1.0]);
    frame.outline(
        panel.x + 3.0,
        panel.y + 3.0,
        panel.w - 6.0,
        panel.h - 6.0,
        1.0,
        [0.15, 0.18, 0.24, 1.0],
    );
    frame.title_text(panel.x + 30.0, panel.y + 22.0, 2.2, [0.98, 0.99, 1.0, 1.0], title);
    let version = format!("OpenBlock v{}", ob_core::VERSION);
    let width = frame.text_width(1.0, &version);
    frame.text(
        panel.x + panel.w - width - 30.0,
        panel.y + 28.0,
        1.0,
        [0.64, 0.70, 0.79, 1.0],
        &version,
    );
    frame.rect(
        panel.x + 30.0,
        panel.y + 58.0,
        panel.w - 60.0,
        1.0,
        [0.24, 0.28, 0.35, 1.0],
    );
}

fn draw_menu_button(
    frame: &mut UiFrame,
    rect: UiRect,
    label: &str,
    selected: bool,
    hovered: bool,
    enabled: bool,
    danger: bool,
) {
    let active = (selected || hovered) && enabled;
    let background = match (enabled, active, danger) {
        (false, _, _) => [0.08, 0.09, 0.12, 0.86],
        (true, true, true) => [0.30, 0.11, 0.11, 0.98],
        (true, true, false) => [0.16, 0.23, 0.32, 0.99],
        (true, false, true) => [0.13, 0.08, 0.09, 0.96],
        (true, false, false) => [0.09, 0.12, 0.17, 0.96],
    };
    let border = match (enabled, active, danger) {
        (false, _, _) => [0.20, 0.23, 0.28, 0.8],
        (true, true, true) => [0.98, 0.56, 0.45, 1.0],
        (true, true, false) => [0.95, 0.79, 0.35, 1.0],
        (true, false, true) => [0.52, 0.27, 0.27, 0.95],
        (true, false, false) => [0.36, 0.42, 0.51, 0.95],
    };
    frame.rect(rect.x, rect.y, rect.w, rect.h, background);
    frame.outline(rect.x, rect.y, rect.w, rect.h, if active { 2.0 } else { 1.0 }, border);
    if active {
        frame.rect(rect.x, rect.y + 5.0, 3.0, rect.h - 10.0, border);
    }
    let color = if enabled {
        [0.96, 0.98, 1.0, 1.0]
    } else {
        [0.46, 0.51, 0.59, 1.0]
    };
    let label_y = rect.y + (rect.h - ob_render::line_height(1.5)) / 2.0;
    frame.text_centered(rect.center().0, label_y, 1.5, color, label);
}

fn draw_text_field(frame: &mut UiFrame, rect: UiRect, text: &str, placeholder: &str, active: bool, cursor: (f32, f32)) {
    let hovered = rect.contains(cursor.0, cursor.1);
    let background = if active {
        [0.10, 0.14, 0.20, 0.99]
    } else if hovered {
        [0.08, 0.11, 0.16, 0.97]
    } else {
        [0.06, 0.08, 0.11, 0.95]
    };
    frame.rect(rect.x, rect.y, rect.w, rect.h, background);
    frame.outline(
        rect.x,
        rect.y,
        rect.w,
        rect.h,
        if active { 2.0 } else { 1.0 },
        if active {
            [0.95, 0.79, 0.35, 1.0]
        } else {
            [0.34, 0.40, 0.49, 0.95]
        },
    );
    let shown = if text.is_empty() { placeholder } else { text };
    let color = if text.is_empty() {
        [0.45, 0.51, 0.60, 1.0]
    } else {
        [0.94, 0.97, 1.0, 1.0]
    };
    let text_y = rect.y + (rect.h - ob_render::line_height(1.4)) / 2.0;
    let width = frame.text(rect.x + 12.0, text_y, 1.4, color, shown);
    if active {
        let caret_x = (rect.x + 12.0 + width + 1.0).min(rect.x + rect.w - 6.0);
        frame.rect(caret_x, rect.y + 8.0, 2.0, rect.h - 16.0, [0.98, 0.85, 0.48, 1.0]);
    }
}

fn draw_world_row(frame: &mut UiFrame, rect: UiRect, world: &WorldSummary, selected: bool, hovered: bool) {
    let background = if selected {
        [0.16, 0.22, 0.31, 0.98]
    } else if hovered {
        [0.12, 0.16, 0.23, 0.95]
    } else {
        [0.07, 0.09, 0.13, 0.85]
    };
    frame.rect(rect.x, rect.y, rect.w, rect.h, background);
    if selected {
        frame.outline(rect.x, rect.y, rect.w, rect.h, 1.0, [0.95, 0.79, 0.35, 0.95]);
        frame.rect(rect.x, rect.y + 4.0, 3.0, rect.h - 8.0, [0.96, 0.76, 0.32, 1.0]);
    }
    let name_chars = (((rect.w - 14.0 - 74.0 - 8.0) / ob_render::glyph_advance(1.45)).max(8.0)) as usize;
    frame.text(
        rect.x + 14.0,
        rect.y + 4.0,
        1.45,
        if selected {
            [1.0, 0.98, 0.92, 1.0]
        } else {
            [0.90, 0.94, 0.99, 1.0]
        },
        &shorten_text(&world.name, name_chars),
    );
    let now = SystemTime::now();
    let meta = format!("{}    modified {}", world.size_label(), world.modified_label(now));
    frame.text(rect.x + 14.0, rect.y + 21.0, 0.95, [0.61, 0.69, 0.80, 1.0], &meta);
    frame.text(
        rect.x + rect.w - 74.0,
        rect.y + 9.0,
        1.0,
        [0.58, 0.67, 0.79, 1.0],
        "Play >",
    );
}

/// Returns whether two clicks at `index` are close enough in time and place.
pub fn is_double_click(previous: Option<(SystemTime, usize)>, now: SystemTime, index: usize) -> bool {
    previous.is_some_and(|(last, last_index)| {
        last_index == index && now.duration_since(last).is_ok_and(|elapsed| elapsed <= DOUBLE_CLICK)
    })
}

/// A deterministic clock used by tests and screenshot seeding.
pub fn synthetic_time(seconds_since_epoch: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds_since_epoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "openblock-frontend-{}-{label}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn make_world(data_dir: &Path, name: &str, size: usize) -> PathBuf {
        let path = data_dir.join("worlds").join(name);
        fs::create_dir_all(&path).expect("world dir");
        let mut file = fs::File::create(path.join(WORLD_MANIFEST)).expect("manifest");
        file.write_all(&vec![0u8; size]).expect("write manifest");
        path
    }

    #[test]
    fn title_continue_is_disabled_without_a_last_world() {
        let dir = temp_dir("title-disabled");
        let mut menu = TitleMenu::new(continue_target(&dir, None));
        assert_eq!(menu.selected, 1, "New World is focused");
        assert!(!menu.enabled(TitleButton::Continue));
        assert_eq!(menu.activate(), Some(TitleButton::NewWorld));
        // Moving up skips the disabled Continue entry.
        menu.move_selection(-1);
        assert_eq!(menu.selected_button(), Some(TitleButton::Quit));
        menu.move_selection(1);
        assert_eq!(menu.selected_button(), Some(TitleButton::NewWorld));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn title_continue_uses_the_last_world_and_needs_its_directory() {
        let dir = temp_dir("title-continue");
        make_world(&dir, "survival", 16);
        let target = continue_target(&dir, Some("survival"));
        assert_eq!(target.as_deref(), Some("survival"));
        let menu = TitleMenu::new(target);
        assert_eq!(menu.selected_button(), Some(TitleButton::Continue));
        assert_eq!(menu.activate(), Some(TitleButton::Continue));
        assert_eq!(continue_target(&dir, Some("deleted")), None);
        assert_eq!(continue_target(&dir, None), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn title_screen_combines_bitmap_headings_with_ui_body_text() {
        let menu = TitleMenu::new(Some("harbor".to_string()));
        let screen = [1280.0, 720.0];
        let mut frame = UiFrame::new(screen[0], screen[1]);
        draw_title_screen(&mut frame, &menu, (-100.0, -100.0), screen);
        let debug = format!("{frame:?}");
        assert!(
            debug.contains("texture: Font"),
            "the OPENBLOCK logo/heading must use the bitmap atlas"
        );
        assert!(
            debug.contains("texture: UiFont"),
            "buttons and footer text must use the UI font atlas"
        );
    }

    #[test]
    fn title_navigation_wraps_and_mouse_clicks_ignore_disabled_buttons() {
        let mut menu = TitleMenu::new(None);
        let count = TitleButton::ALL.len();
        menu.selected = 0;
        menu.move_selection(1);
        assert_eq!(menu.selected, 1);
        menu.move_selection(-1);
        assert_eq!(menu.selected, count - 1, "disabled Continue is skipped backwards");
        assert_eq!(menu.click(TitleButton::Continue), None);
        assert_eq!(menu.selected_button(), Some(TitleButton::Quit));
        assert_eq!(menu.click(TitleButton::Settings), Some(TitleButton::Settings));
        assert_eq!(menu.selected_button(), Some(TitleButton::Settings));

        let rects = title_button_rects([1280.0, 720.0]);
        for (index, rect) in rects.iter().enumerate() {
            assert!(rect.x >= 0.0 && rect.y >= 0.0);
            assert!(rect.x + rect.w <= 1280.0);
            assert!(rect.y + rect.h <= 720.0);
            if index == 0 {
                assert_eq!(menu.update_hover([1280.0, 720.0], rect.center()), None);
            } else {
                assert_eq!(
                    menu.update_hover([1280.0, 720.0], rect.center()),
                    TitleButton::ALL.get(index).copied()
                );
            }
        }
        for screen in [[800.0, 600.0], [420.0, 300.0], [1920.0, 1080.0]] {
            for rect in title_button_rects(screen) {
                assert!(rect.x >= 0.0 && rect.y >= 0.0);
                assert!(rect.x + rect.w <= screen[0]);
                assert!(rect.y + rect.h <= screen[1]);
            }
        }
    }

    #[test]
    fn world_name_validation_accepts_normal_names() {
        assert_eq!(validate_world_name("My world"), Ok("My world".to_string()));
        assert_eq!(
            validate_world_name(" survival_2026-1 "),
            Ok("survival_2026-1".to_string())
        );
        assert_eq!(
            validate_world_name(&"a".repeat(MAX_WORLD_NAME_CHARS)),
            Ok("a".repeat(MAX_WORLD_NAME_CHARS))
        );
        assert_eq!(validate_world_name("My.World"), Ok("My.World".to_string()));
    }

    #[test]
    fn world_name_validation_rejects_unsafe_names() {
        for (name, reason) in [
            ("", "empty"),
            ("   ", "blank"),
            ("../escape", "slash"),
            ("a/b", "slash"),
            ("a\\b", "backslash"),
            ("bad:name", "colon"),
            ("bad*name", "star"),
            ("bad?", "question"),
            ("trailing.", "trailing dot"),
            (".", "dot"),
            ("..", "dotdot"),
            ("CON", "reserved"),
            ("nul.txt", "reserved"),
            ("æland", "non ascii"),
            ("bad\u{7}", "control"),
        ] {
            assert!(validate_world_name(name).is_err(), "{reason}: {name:?}");
        }
        let too_long = "a".repeat(MAX_WORLD_NAME_CHARS + 1);
        assert!(validate_world_name(&too_long).is_err());
    }

    #[test]
    fn seed_validation_defaults_empty_to_zero_and_rejects_bad_numbers() {
        assert_eq!(parse_seed(""), Ok(0));
        assert_eq!(parse_seed("   "), Ok(0));
        assert_eq!(parse_seed("42"), Ok(42));
        assert_eq!(parse_seed("18446744073709551615"), Ok(u64::MAX));
        assert!(parse_seed("-1").is_err());
        assert!(parse_seed("18446744073709551616").is_err());
        assert!(parse_seed("abc").is_err());
        assert!(parse_seed("1.5").is_err());
    }

    #[test]
    fn new_world_form_edits_validates_and_detects_existing_worlds() {
        let dir = temp_dir("new-world");
        let mut form = NewWorldForm::default();
        assert_eq!(form.name, "My world");
        form.next_field();
        assert_eq!(form.field, NewWorldField::Seed);
        assert!(form.insert_char('9'));
        assert!(form.insert_char('9'));
        assert!(!form.insert_char('a'));
        form.previous_field();
        form.backspace();
        form.insert_char('s');
        assert!(form.name.ends_with('s'), "typing appends allowed ASCII characters");
        let request = form.validate(&dir).expect("valid form");
        assert_eq!(request.name, "My worls");
        assert_eq!(request.seed, 99);

        form.error = Some("previous".to_string());
        form.insert_char('x');
        assert_eq!(form.error, None);
        form.backspace();

        make_world(&dir, "My worls", 8);
        assert!(form.validate(&dir).is_err(), "existing worlds must not be overwritten");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn discover_worlds_lists_only_world_directories_and_sorts_recent_first() {
        let dir = temp_dir("discover");
        make_world(&dir, "older", 10);
        make_world(&dir, "newer", 2_048);
        fs::create_dir_all(dir.join("worlds").join("not-a-world")).expect("decoy");
        fs::create_dir_all(dir.join("worlds").join("file-instead")).expect("decoy dir");
        fs::write(dir.join("worlds").join("file-instead").join(WORLD_MANIFEST), b"x").expect("file");
        fs::write(dir.join("worlds").join("stray.obs"), b"x").expect("stray");

        // Deterministic modification times: newer is 2026-11-25, older is
        // 2026-11-24 (the exact dates do not matter, only their order).
        let newer_time = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let older_time = newer_time - Duration::from_secs(86_400);
        for (name, time) in [("older", older_time), ("newer", newer_time)] {
            let file = fs::File::options()
                .write(true)
                .open(dir.join("worlds").join(name).join(WORLD_MANIFEST))
                .expect("open");
            let times = fs::FileTimes::new().set_modified(time);
            file.set_times(times).expect("set mtime");
        }
        let worlds = discover_worlds(&dir);
        let names: Vec<&str> = worlds.iter().map(|world| world.name.as_str()).collect();
        assert_eq!(names, vec!["newer", "older", "file-instead"]);
        assert_eq!(worlds[0].size_bytes, 2_048);
        assert_eq!(worlds[1].size_bytes, 10);
        assert!(worlds.iter().all(|world| world.path.join(WORLD_MANIFEST).is_file()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn world_list_selection_scroll_and_delete_confirmation_are_bounded() {
        let dir = temp_dir("world-list");
        for name in ["a", "b", "c", "d"] {
            make_world(&dir, name, 8);
        }
        let mut list = WorldList::new(discover_worlds(&dir));
        assert_eq!(list.worlds.len(), 4);
        list.move_selection(-1, 2);
        assert_eq!(list.selected, 3, "wraps to the last row");
        assert_eq!(list.scroll, 2, "keeps the selection visible");
        list.move_selection(1, 2);
        assert_eq!(list.selected, 0);
        assert_eq!(list.scroll, 0);
        let selected = list.request_delete().expect("selected");
        assert!(list.confirm_delete.as_deref() == Some(selected.as_str()));
        list.cancel_delete();
        assert!(list.confirm_delete.is_none());

        list.confirm_delete = Some("b".to_string());
        list.confirm_delete(&dir).expect("delete b");
        let mut names: Vec<&str> = list.worlds.iter().map(|world| world.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["a", "c", "d"]);
        assert!(!dir.join("worlds").join("b").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn delete_world_refuses_paths_outside_the_worlds_directory() {
        let dir = temp_dir("delete-refuse");
        make_world(&dir, "safe", 8);
        fs::write(dir.join("worlds").join("plain.txt"), b"x").expect("file");
        assert!(delete_world(&dir, "../escape").is_err());
        assert!(delete_world(&dir, "missing").is_err());
        assert!(delete_world(&dir, "plain.txt").is_err());
        delete_world(&dir, "safe").expect("safe world deletion");
        assert!(!dir.join("worlds").join("safe").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn size_and_modified_labels_are_human_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1_023), "1023 B");
        assert_eq!(format_size(1_024), "1.0 KB");
        assert_eq!(format_size(5_242_880), "5.0 MB");
        let now = synthetic_time(1_800_000_000);
        assert_eq!(format_modified(now, now), "just now");
        assert_eq!(format_modified(now - Duration::from_secs(120), now), "2 min ago");
        assert_eq!(format_modified(now - Duration::from_secs(3 * 3_600), now), "3 h ago");
        assert_eq!(format_modified(now - Duration::from_secs(2 * 86_400), now), "2 d ago");
    }

    #[test]
    fn server_address_validation_accepts_ipv4_hostnames_and_bracketed_ipv6() {
        assert_eq!(
            validate_server_address("127.0.0.1:25565"),
            Ok("127.0.0.1:25565".to_string())
        );
        assert_eq!(
            validate_server_address(" example.com:25565 "),
            Ok("example.com:25565".to_string())
        );
        assert_eq!(
            validate_server_address("mc.example-host_1:19132"),
            Ok("mc.example-host_1:19132".to_string())
        );
        assert_eq!(validate_server_address("[::1]:25565"), Ok("[::1]:25565".to_string()));
    }

    #[test]
    fn server_address_validation_rejects_malformed_endpoints() {
        for (address, reason) in [
            ("", "empty"),
            ("localhost", "no port"),
            ("localhost:", "empty port"),
            (":25565", "empty host"),
            ("host:0", "zero port"),
            ("host:65536", "port too large"),
            ("host:abc", "non numeric port"),
            ("ho st:25565", "space"),
            ("host name:25565", "space"),
            ("[::1]25565", "missing colon"),
            ("[::1:25565", "unclosed bracket"),
            ("::1:25565", "bare ipv6"),
            ("host\n:25565", "control"),
            ("bad/host:25565", "slash"),
        ] {
            assert!(validate_server_address(address).is_err(), "{reason}: {address:?}");
        }
        assert!(validate_server_address(&format!("host:{}", "1".repeat(300))).is_err());
    }

    #[test]
    fn player_name_validation_matches_the_protocol_rule() {
        assert_eq!(validate_player_name(" alice "), Ok("alice".to_string()));
        for invalid in ["", "al ice", "ælice", "a".repeat(33).as_str()] {
            assert!(validate_player_name(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn multiplayer_form_edits_and_validates() {
        let mut form = MultiplayerForm::new();
        assert_eq!(form.address, DEFAULT_SERVER_ADDRESS);
        assert!(!form.name.is_empty());
        form.address.clear();
        for character in "127.0.0.1:25566".chars() {
            assert!(form.insert_char(character));
        }
        assert_eq!(form.validate().unwrap().address, "127.0.0.1:25566");
        form.next_field(0);
        assert_eq!(form.field, MultiplayerField::Name);
        form.backspace();
        form.insert_char('!');
        assert!(form.validate().is_err(), "invalid player name is rejected");
        form.insert_char('x');

        let recent = RecentServer {
            address: "mc.example.com:25565".to_string(),
            name: "alice".to_string(),
        };
        form.select_recent(&recent);
        assert_eq!(form.address, "mc.example.com:25565");
        assert_eq!(form.name, "alice");
        assert_eq!(form.validate().unwrap().name, "alice");
    }

    #[test]
    fn multiplayer_focus_skips_recents_when_the_list_is_empty() {
        let mut form = MultiplayerForm::new();
        form.next_field(0);
        assert_eq!(form.field, MultiplayerField::Name);
        form.next_field(0);
        assert_eq!(form.field, MultiplayerField::Address);
        form.previous_field(0);
        assert_eq!(form.field, MultiplayerField::Name);
        form.field = MultiplayerField::Address;
        form.previous_field(3);
        assert_eq!(form.field, MultiplayerField::Recents);
        form.move_recent(-1, 3);
        assert_eq!(form.recent_selected, 2);
        form.move_recent(1, 3);
        assert_eq!(form.recent_selected, 0);
    }

    #[test]
    fn menu_layouts_stay_on_screen_and_do_not_overlap_buttons() {
        for screen in [[1280.0, 720.0], [800.0, 600.0], [420.0, 300.0], [1920.0, 1080.0]] {
            let panel = menu_panel_rect(screen);
            assert!(panel.x >= 0.0 && panel.y >= 0.0);
            assert!(panel.x + panel.w <= screen[0]);
            assert!(panel.y + panel.h <= screen[1]);
            let [create, back] = form_button_rects(screen);
            assert!(create.y + create.h <= panel.y + panel.h);
            assert!(back.y + back.h <= panel.y + panel.h);
            let [play, delete, cancel] = world_list_button_rects(screen);
            for button in [play, delete, cancel] {
                assert!(button.y + button.h <= panel.y + panel.h);
                assert!(button.x >= panel.x && button.x + button.w <= panel.x + panel.w);
            }
            assert!(!play.intersects(delete) && !delete.intersects(cancel));
            let fields = new_world_field_rects(screen);
            assert!(fields[1].y + fields[1].h < create.y);
            let rows = visible_row_rects(screen, world_row_capacity(screen));
            assert!(!rows.is_empty());
            assert!(rows.last().is_none_or(|row| row.y + row.h <= play.y));
            let [delete_confirm, cancel_confirm] = delete_confirm_button_rects(screen);
            assert!(!delete_confirm.intersects(cancel_confirm));
            for rect in recent_server_rects(screen) {
                assert!(rect.x >= panel.x && rect.x + rect.w <= panel.x + panel.w);
            }
        }
    }

    #[test]
    fn panels_clear_the_footer_band_at_1280x720_and_960x540() {
        for screen in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            let panel = menu_panel_rect(screen);
            assert!(
                panel.y + panel.h <= screen[1] - 44.0,
                "panel bottom {} overlaps the footer band at {screen:?}",
                panel.y + panel.h
            );
            let fields = multiplayer_field_rects(screen);
            assert!(
                fields[1].y - 20.0 >= fields[0].y + fields[0].h + 4.0,
                "player-name label crowds the address field at {screen:?}"
            );
            let [create, _] = form_button_rects(screen);
            let new_fields = new_world_field_rects(screen);
            assert!(
                new_fields[1].y + new_fields[1].h + 8.0 <= create.y,
                "seed field crowds Create at {screen:?}"
            );
        }
    }

    #[test]
    fn multiplayer_error_line_clears_the_connect_button() {
        for screen in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            let [connect, _] = form_button_rects(screen);
            let y = multiplayer_error_y(screen);
            assert!(
                y + ob_render::line_height(1.15) <= connect.y - 4.0,
                "error line overlaps Connect at {screen:?}"
            );
            let fields = multiplayer_field_rects(screen);
            assert!(
                y >= fields[1].y + fields[1].h,
                "error line overlaps the form at {screen:?}"
            );
        }
    }

    #[test]
    fn double_click_detection_uses_time_and_row() {
        let now = SystemTime::now();
        assert!(!is_double_click(None, now, 0));
        assert!(is_double_click(Some((now, 2)), now, 2));
        assert!(!is_double_click(Some((now, 2)), now, 1));
        assert!(!is_double_click(
            Some((now - DOUBLE_CLICK - Duration::from_millis(1), 2)),
            now,
            2
        ));
    }

    #[test]
    fn scaled_pointer_mapping_hits_title_and_form_buttons() {
        use crate::ui_scale::{physical_to_ui_point, ui_scale_factor, ui_screen, ui_to_physical_point};
        for physical in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            for percent in [75_u16, 100, 125, 150] {
                let screen = ui_screen(physical, percent);
                let factor = ui_scale_factor(percent, physical[1]);
                let mut title = TitleMenu::new(Some("harbor".to_string()));
                for (index, rect) in title_button_rects(screen).iter().enumerate() {
                    let point = physical_to_ui_point(ui_to_physical_point(rect.center(), factor), factor);
                    assert_eq!(
                        title.update_hover(screen, point),
                        TitleButton::ALL.get(index).copied(),
                        "{physical:?} at {percent}%"
                    );
                }
                for rect in form_button_rects(screen) {
                    let point = physical_to_ui_point(ui_to_physical_point(rect.center(), factor), factor);
                    assert!(rect.contains(point.0, point.1));
                }
                // New-world and multiplayer field clicks use the same mapping.
                for rect in new_world_field_rects(screen)
                    .into_iter()
                    .chain(multiplayer_field_rects(screen))
                {
                    let point = physical_to_ui_point(ui_to_physical_point(rect.center(), factor), factor);
                    assert!(rect.contains(point.0, point.1));
                }
            }
        }
    }
}

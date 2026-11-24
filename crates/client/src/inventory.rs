//! Creative inventory model: registry-backed block list, responsive grid,
//! hover/selection state, hotbar assignment and drawing.
//!
//! The layout and state-machine code is GPU-free and unit-testable; only
//! [`draw`] touches the immediate-mode [`UiFrame`]. The block list always comes
//! from the session [`Registry`], so data packs and future block additions show
//! up without client changes.

use ob_content::{BlockId, Registry};
use ob_render::UiFrame;

use crate::ui_state::{BLOCK_NAME_SECONDS, HOTBAR_SLOTS, UiRect, UiState};

/// Number of columns in the inventory grid.
pub const INVENTORY_COLUMNS: usize = 9;
/// Preferred slot edge length in UI pixels.
pub const INVENTORY_SLOT_SIZE: f32 = 48.0;
/// Gap between inventory slots.
pub const INVENTORY_GAP: f32 = 4.0;
/// Horizontal padding inside the inventory panel.
pub const INVENTORY_PADDING: f32 = 24.0;
/// Space reserved for the title above the grid.
pub const INVENTORY_TITLE_HEIGHT: f32 = 48.0;
/// Space reserved for the hovered-name footer below the grid.
pub const INVENTORY_FOOTER_HEIGHT: f32 = 76.0;
/// Minimum distance between the panel and the screen edge.
pub const INVENTORY_MARGIN: f32 = 24.0;
/// Smallest slot edge length used on very narrow windows.
const MIN_SLOT_SIZE: f32 = 8.0;

/// One visible inventory slot: its position in the full block list and its rect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InventorySlot {
    /// Index into the registry-backed block list.
    pub index: usize,
    /// Drawable/hit-testable rectangle.
    pub rect: UiRect,
}

/// Computed geometry of the inventory screen.
#[derive(Debug, Clone, PartialEq)]
pub struct InventoryLayout {
    /// Centred panel rectangle.
    pub panel: UiRect,
    /// Visible slots in row-major order.
    pub slots: Vec<InventorySlot>,
    /// First visible grid row (each row holds [`INVENTORY_COLUMNS`] items).
    pub scroll: usize,
    /// Number of grid rows that fit inside the panel.
    pub visible_rows: usize,
    /// Total number of grid rows needed for every item.
    pub total_rows: usize,
    /// Slot edge length after responsive shrinking.
    pub slot_size: f32,
}

impl InventoryLayout {
    /// Maps a UI point to an item index, or `None` outside every visible slot.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        self.slots
            .iter()
            .find(|slot| slot.rect.contains(x, y))
            .map(|slot| slot.index)
    }

    /// Rectangle of a specific item index if it is currently visible.
    pub fn slot_rect(&self, index: usize) -> Option<UiRect> {
        self.slots.iter().find(|slot| slot.index == index).map(|slot| slot.rect)
    }
}

/// Selection, hover and scroll state of the inventory screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InventoryState {
    /// Keyboard-selected item index into the registry-backed block list.
    pub selected: usize,
    /// Item index under the mouse cursor, if any.
    pub hovered: Option<usize>,
    /// First visible grid row.
    pub scroll: usize,
}

impl InventoryState {
    /// Resets selection, hover and scroll for a freshly opened inventory.
    pub fn open(&mut self) {
        self.selected = 0;
        self.hovered = None;
        self.scroll = 0;
    }

    /// Updates the hovered item from a cursor position.
    pub fn update_hover(&mut self, cursor: (f32, f32), screen: [f32; 2], item_count: usize) -> Option<usize> {
        let layout = layout(screen, item_count, self.scroll);
        self.scroll = layout.scroll;
        self.hovered = layout.hit_test(cursor.0, cursor.1);
        self.hovered
    }

    /// Moves the keyboard selection by grid columns/rows and keeps it visible.
    pub fn move_selection(&mut self, screen: [f32; 2], item_count: usize, dx: i32, dy: i32) {
        if item_count == 0 {
            self.selected = 0;
            self.hovered = None;
            self.scroll = 0;
            return;
        }
        self.selected = self.selected.min(item_count - 1);
        let delta = i64::from(dx) + i64::from(dy) * INVENTORY_COLUMNS as i64;
        self.selected = (self.selected as i64 + delta).clamp(0, item_count as i64 - 1) as usize;

        let layout = layout(screen, item_count, self.scroll);
        let selected_row = self.selected / INVENTORY_COLUMNS;
        let visible_rows = layout.visible_rows.max(1);
        if selected_row < layout.scroll {
            self.scroll = selected_row;
        } else if selected_row >= layout.scroll + visible_rows {
            self.scroll = selected_row + 1 - visible_rows;
        }
        let max_scroll = layout.total_rows.saturating_sub(layout.visible_rows);
        self.scroll = self.scroll.min(max_scroll);
    }

    /// Scrolls by `rows` (positive = later rows), clamped to the visible range.
    pub fn scroll_by(&mut self, screen: [f32; 2], item_count: usize, rows: i32) {
        let layout = layout(screen, item_count, self.scroll);
        let max_scroll = layout.total_rows.saturating_sub(layout.visible_rows);
        self.scroll = (layout.scroll as i64 + i64::from(rows)).clamp(0, max_scroll as i64) as usize;
    }

    /// Index selected by Enter/click, if the selection points at an item.
    pub fn activate(&self, item_count: usize) -> Option<usize> {
        (self.selected < item_count).then_some(self.selected)
    }
}

/// Every registered block except air, in registry order.
pub fn inventory_blocks(registry: &Registry) -> Vec<BlockId> {
    registry
        .iter()
        .filter(|(id, _)| *id != BlockId::AIR)
        .map(|(id, _)| id)
        .collect()
}

/// Human-readable block name: namespace stripped, underscores as spaces and
/// each word title-cased (`openblock:oak_log` becomes `Oak Log`).
pub fn display_name(resource_id: &str) -> String {
    let name = resource_id.split_once(':').map(|(_, name)| name).unwrap_or(resource_id);
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            let mut title = String::new();
            title.extend(first.to_uppercase());
            title.push_str(&chars.as_str().to_lowercase());
            title
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Hardness hint shown under the hovered block name.
pub fn hardness_hint(hardness: f32) -> String {
    if hardness.is_finite() {
        format!("hardness {hardness:.1} s")
    } else {
        "unbreakable".to_string()
    }
}

/// Computes the centred panel and the visible slice of a 9-column grid.
///
/// The grid shows at most as many rows as fit between the title and footer on
/// the current screen. Extra rows are reachable by scrolling; `scroll` is a row
/// offset and is clamped to the final page.
pub fn layout(screen: [f32; 2], item_count: usize, scroll: usize) -> InventoryLayout {
    let width = screen[0].max(1.0);
    let height = screen[1].max(1.0);
    let gap = INVENTORY_GAP;

    // Shrink slots on narrow windows so nine columns always fit where possible.
    let max_grid_width = (width - 2.0 * INVENTORY_MARGIN - 2.0 * INVENTORY_PADDING).max(1.0);
    let column_count = INVENTORY_COLUMNS as f32;
    let slot_from_width = (max_grid_width - gap * (column_count - 1.0)) / column_count;
    let slot_size = slot_from_width.clamp(MIN_SLOT_SIZE, INVENTORY_SLOT_SIZE);
    let grid_width = column_count * slot_size + gap * (column_count - 1.0);
    let panel_width = grid_width + 2.0 * INVENTORY_PADDING;

    // Fit as many rows as possible; panel height follows the visible rows.
    let max_panel_height = (height - 2.0 * INVENTORY_MARGIN).max(1.0);
    let available_grid_height = (max_panel_height - INVENTORY_TITLE_HEIGHT - INVENTORY_FOOTER_HEIGHT).max(0.0);
    let row_pitch = slot_size + gap;
    let rows_fit = (((available_grid_height + gap) / row_pitch).floor() as usize).max(1);

    let total_rows = item_count.div_ceil(INVENTORY_COLUMNS);
    let visible_rows = total_rows.min(rows_fit);
    let max_scroll = total_rows.saturating_sub(visible_rows);
    let scroll = scroll.min(max_scroll);

    let visible_height = visible_rows as f32 * slot_size + visible_rows.saturating_sub(1) as f32 * gap;
    let panel_height = INVENTORY_TITLE_HEIGHT + visible_height + INVENTORY_FOOTER_HEIGHT;
    let panel = UiRect::new(
        (width - panel_width) / 2.0,
        (height - panel_height) / 2.0,
        panel_width,
        panel_height,
    );
    let grid_x = panel.x + INVENTORY_PADDING;
    let grid_y = panel.y + INVENTORY_TITLE_HEIGHT;

    let mut slots = Vec::new();
    for row in 0..visible_rows {
        for column in 0..INVENTORY_COLUMNS {
            let index = (scroll + row) * INVENTORY_COLUMNS + column;
            if index >= item_count {
                break;
            }
            slots.push(InventorySlot {
                index,
                rect: UiRect::new(
                    grid_x + column as f32 * (slot_size + gap),
                    grid_y + row as f32 * (slot_size + gap),
                    slot_size,
                    slot_size,
                ),
            });
        }
    }

    InventoryLayout {
        panel,
        slots,
        scroll,
        visible_rows,
        total_rows,
        slot_size,
    }
}

/// Assigns the keyboard/mouse selection to the selected hotbar slot and closes
/// the inventory. Returns the assigned block id, or `None` for an empty list.
pub fn assign_selected_to_hotbar(ui: &mut UiState, registry: &Registry) -> Option<BlockId> {
    let items = inventory_blocks(registry);
    let index = ui.inventory.activate(items.len())?;
    let block = registry.block(items[index]);
    let slot = ui.hotbar_selected % HOTBAR_SLOTS;
    ui.hotbar_selected = slot;
    ui.hotbar[slot] = block.id;
    ui.block_name_seconds = BLOCK_NAME_SECONDS;
    ui.close_inventory();
    Some(items[index])
}

/// Draws the inventory overlay (dimmed background, centred panel, grid and
/// hovered-block footer) into `frame`.
pub fn draw(frame: &mut UiFrame, ui: &UiState, registry: &Registry, screen: [f32; 2]) {
    let items = inventory_blocks(registry);
    let layout = layout(screen, items.len(), ui.inventory.scroll);
    let white = [1.0, 1.0, 1.0, 1.0];
    let muted = [0.78, 0.83, 0.90, 1.0];

    frame.rect(0.0, 0.0, screen[0], screen[1], [0.0, 0.0, 0.0, 0.55]);
    frame.rect(
        layout.panel.x,
        layout.panel.y,
        layout.panel.w,
        layout.panel.h,
        [0.07, 0.08, 0.11, 0.96],
    );
    frame.outline(
        layout.panel.x,
        layout.panel.y,
        layout.panel.w,
        layout.panel.h,
        2.0,
        [0.42, 0.48, 0.56, 1.0],
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        layout.panel.y + 14.0,
        2.5,
        white,
        "Blocks",
    );

    for slot in &layout.slots {
        let block = registry.block(items[slot.index]);
        let selected = slot.index == ui.inventory.selected;
        let hovered = ui.inventory.hovered == Some(slot.index);
        let background = if hovered {
            [0.25, 0.30, 0.38, 0.98]
        } else {
            [0.11, 0.13, 0.17, 0.94]
        };
        frame.rect(slot.rect.x, slot.rect.y, slot.rect.w, slot.rect.h, background);
        let (thickness, border) = if selected {
            (2.0, [1.0, 0.92, 0.45, 1.0])
        } else if hovered {
            (2.0, [0.88, 0.92, 0.98, 1.0])
        } else {
            (1.0, [0.32, 0.36, 0.43, 0.95])
        };
        frame.outline(slot.rect.x, slot.rect.y, slot.rect.w, slot.rect.h, thickness, border);
        let icon_size = slot.rect.w - 12.0;
        if icon_size > 4.0 {
            frame.block_icon(
                slot.rect.x + 6.0,
                slot.rect.y + 5.0,
                icon_size,
                u32::from(block.textures.top.0),
                u32::from(block.textures.side.0),
            );
        }
    }

    let footer_top = layout.panel.y + layout.panel.h - INVENTORY_FOOTER_HEIGHT;
    frame.rect(
        layout.panel.x + INVENTORY_PADDING,
        footer_top,
        layout.panel.w - 2.0 * INVENTORY_PADDING,
        1.0,
        [0.30, 0.34, 0.40, 0.9],
    );

    let info_index = ui
        .inventory
        .hovered
        .or(Some(ui.inventory.selected))
        .filter(|index| *index < items.len());
    let name = info_index
        .map(|index| display_name(registry.block(items[index]).id))
        .unwrap_or_else(|| "No blocks".to_string());
    let hint = info_index
        .map(|index| hardness_hint(registry.block(items[index]).hardness))
        .unwrap_or_else(|| "The registry is empty".to_string());
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + 8.0,
        2.0,
        white,
        &name,
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + 30.0,
        1.2,
        muted,
        &hint,
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + 52.0,
        1.0,
        [0.68, 0.74, 0.82, 1.0],
        "Click/Enter assign   Arrows select   Wheel scroll   E/Esc close",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_state::Screen;

    #[test]
    fn inventory_lists_every_registered_block_except_air() {
        let registry = Registry::base();
        let items = inventory_blocks(&registry);
        assert_eq!(items.len(), registry.len() - 1);
        assert_eq!(items.len(), 11);
        assert!(!items.contains(&BlockId::AIR));
        assert!(items.iter().any(|id| registry.block(*id).id == "openblock:snow"));
        assert!(items.iter().any(|id| registry.block(*id).id == "openblock:gravel"));
        assert!(items.iter().any(|id| registry.block(*id).id == "openblock:clay"));
    }

    #[test]
    fn layout_is_centred_with_nine_columns_and_slots_inside_the_panel() {
        let screen = [1280.0, 720.0];
        let layout = layout(screen, 11, 0);
        assert_eq!(layout.slots.len(), 11);
        assert_eq!(layout.total_rows, 2);
        assert_eq!(layout.visible_rows, 2);
        let centre = layout.panel.center();
        assert!((centre.0 - 640.0).abs() < 0.01);
        assert!((centre.1 - 360.0).abs() < 0.01);

        assert_eq!(layout.slots[0].index, 0);
        assert_eq!(layout.slots[8].index, 8);
        assert_eq!(layout.slots[9].index, 9);
        assert!(layout.slots[0].rect.y == layout.slots[8].rect.y);
        assert!(layout.slots[9].rect.y > layout.slots[0].rect.y);
        for slot in &layout.slots {
            assert!(
                layout.panel.contains(slot.rect.x + 0.1, slot.rect.y + 0.1),
                "slot origin must lie inside the panel"
            );
            assert!(
                layout
                    .panel
                    .contains(slot.rect.x + slot.rect.w - 0.1, slot.rect.y + slot.rect.h - 0.1),
                "slot far corner must lie inside the panel"
            );
        }
    }

    #[test]
    fn hit_testing_maps_slots_and_ignores_outside_points() {
        let screen = [1280.0, 720.0];
        let layout = layout(screen, 11, 0);
        let slot = layout.slots[3];
        assert_eq!(layout.hit_test(slot.rect.center().0, slot.rect.center().1), Some(3));
        let slot = layout.slots[10];
        assert_eq!(layout.hit_test(slot.rect.center().0, slot.rect.center().1), Some(10));
        assert_eq!(layout.hit_test(1.0, 1.0), None);
        assert_eq!(layout.slot_rect(10), Some(layout.slots[10].rect));
        assert_eq!(layout.slot_rect(11), None);
    }

    #[test]
    fn scrolling_is_capped_to_the_visible_rows() {
        // A short window fits only a couple of rows, forcing pagination.
        let screen = [800.0, 300.0];
        let layout = layout(screen, 100, usize::MAX);
        assert!(layout.visible_rows < layout.total_rows);
        assert_eq!(layout.scroll, layout.total_rows - layout.visible_rows);
        assert_eq!(
            layout.slots.len(),
            100 - layout.scroll * INVENTORY_COLUMNS,
            "the last page may be partially filled"
        );
        assert_eq!(layout.slots[0].index, layout.scroll * INVENTORY_COLUMNS);
        let first = layout.slots[0];
        assert_eq!(
            layout.hit_test(first.rect.center().0, first.rect.center().1),
            Some(layout.scroll * INVENTORY_COLUMNS)
        );
        assert!(layout.panel.y >= 0.0);
        assert!(layout.panel.y + layout.panel.h <= screen[1]);
    }

    #[test]
    fn arrow_selection_moves_by_grid_step_and_auto_scrolls() {
        let screen = [800.0, 300.0];
        let layout = layout(screen, 100, 0);
        assert_eq!(layout.visible_rows, 2);

        let mut state = InventoryState::default();
        state.move_selection(screen, 100, 0, 1);
        assert_eq!(state.selected, 9);
        assert_eq!(state.scroll, 0);
        state.move_selection(screen, 100, 0, 1);
        assert_eq!(state.selected, 18);
        assert_eq!(state.scroll, 1, "selection must remain visible");
        state.move_selection(screen, 100, 0, 1);
        assert_eq!(state.selected, 27);
        assert_eq!(state.scroll, 2);
        state.move_selection(screen, 100, 0, -1);
        assert_eq!(state.selected, 18);

        // Moving past the ends clamps instead of wrapping.
        state.selected = 99;
        state.move_selection(screen, 100, 0, 1);
        assert_eq!(state.selected, 99);
        state.move_selection(screen, 100, 0, -1);
        assert_eq!(state.selected, 90);
        state.move_selection(screen, 100, -1, 0);
        assert_eq!(state.selected, 89);
        state.move_selection(screen, 100, 1, 0);
        assert_eq!(state.selected, 90);
    }

    #[test]
    fn wheel_scrolling_clamps_to_the_first_and_last_page() {
        let screen = [800.0, 300.0];
        let total = 100;
        let mut state = InventoryState::default();
        state.scroll_by(screen, total, -5);
        assert_eq!(state.scroll, 0);
        state.scroll_by(screen, total, 3);
        assert_eq!(state.scroll, 3);
        state.scroll_by(screen, total, 999);
        let layout = layout(screen, total, state.scroll);
        assert_eq!(state.scroll, layout.total_rows - layout.visible_rows);
        state.scroll_by(screen, total, -999);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn hover_updates_from_cursor_and_reports_names() {
        let registry = Registry::base();
        let items = inventory_blocks(&registry);
        let screen = [1280.0, 720.0];
        let layout = layout(screen, items.len(), 0);
        let index = items
            .iter()
            .position(|id| registry.block(*id).id == "openblock:oak_log")
            .or_else(|| items.iter().position(|id| registry.block(*id).id == "openblock:log"))
            .expect("log is registered");
        let slot = layout.slots[index];
        let mut state = InventoryState::default();
        assert_eq!(state.update_hover(slot.rect.center(), screen, items.len()), Some(index));
        assert_eq!(state.hovered, Some(index));
        assert_eq!(display_name(registry.block(items[index]).id), "Log");
        assert_eq!(state.update_hover((0.0, 0.0), screen, items.len()), None);
    }

    #[test]
    fn display_names_strip_namespace_and_title_case_words() {
        assert_eq!(display_name("openblock:grass"), "Grass");
        assert_eq!(display_name("openblock:oak_log"), "Oak Log");
        assert_eq!(display_name("other_pack:red_snow"), "Red Snow");
        assert_eq!(display_name("stone"), "Stone");
        assert_eq!(display_name("openblock:"), "");
    }

    #[test]
    fn hardness_hints_cover_breakable_and_unbreakable_blocks() {
        assert_eq!(hardness_hint(1.5), "hardness 1.5 s");
        assert_eq!(hardness_hint(0.2), "hardness 0.2 s");
        assert_eq!(hardness_hint(f32::INFINITY), "unbreakable");
    }

    #[test]
    fn assigning_selection_puts_the_block_in_the_hotbar_and_closes() {
        let registry = Registry::base();
        let items = inventory_blocks(&registry);
        let gravel = items
            .iter()
            .position(|id| registry.block(*id).id == "openblock:gravel")
            .expect("gravel is registered");

        let mut ui = UiState::default();
        ui.open_inventory();
        ui.hotbar_selected = 4;
        ui.inventory.selected = gravel;
        assert_eq!(ui.screen, Screen::Inventory);

        let assigned = assign_selected_to_hotbar(&mut ui, &registry).expect("assignment");
        assert_eq!(registry.block(assigned).id, "openblock:gravel");
        assert_eq!(ui.hotbar[4], "openblock:gravel");
        assert_eq!(ui.screen, Screen::Playing);
        assert!(ui.block_name_seconds > 0.0);
    }

    #[test]
    fn empty_registry_list_disables_activation() {
        let registry = Registry::base();
        let empty: Vec<BlockId> = Vec::new();
        let state = InventoryState::default();
        assert_eq!(state.activate(empty.len()), None);
        // A real registry always has at least one non-air block.
        assert!(!inventory_blocks(&registry).is_empty());
    }
}

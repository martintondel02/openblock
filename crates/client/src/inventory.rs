//! Creative inventory model: registry-backed block list, responsive grid,
//! hover/selection state, hotbar assignment and drawing.
//!
//! The layout and state-machine code is GPU-free and unit-testable; only
//! [`draw`] touches the immediate-mode [`UiFrame`]. The block list always comes
//! from the session [`Registry`], so data packs and future block additions show
//! up without client changes.

use ob_content::{BlockId, Registry};
use ob_render::UiFrame;
use ob_server::{GameMode, ItemStack};
use ob_sim::MAX_HUNGER;

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
/// Horizontal padding inside the survival inventory panel.
pub const SURVIVAL_PADDING: f32 = 24.0;
/// Smallest slot edge length used on very narrow windows.
const MIN_SLOT_SIZE: f32 = 8.0;
/// Width of the one-row survival inventory grid, in items.
const SURVIVAL_COLUMNS: usize = HOTBAR_SLOTS;

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
    /// Keyboard-selected item index into the registry-backed block list, or a
    /// hotbar slot index in survival.
    pub selected: usize,
    /// Item index or hotbar slot under the mouse cursor, if any.
    pub hovered: Option<usize>,
    /// First visible grid row (creative palette only).
    pub scroll: usize,
}

/// Computed geometry of the one-row survival inventory.
#[derive(Debug, Clone, PartialEq)]
pub struct SurvivalInventoryLayout {
    /// Centred panel rectangle.
    pub panel: UiRect,
    /// Nine hotbar-slot rectangles in slot order.
    pub slots: Vec<UiRect>,
    /// Slot edge length after responsive shrinking.
    pub slot_size: f32,
}

impl SurvivalInventoryLayout {
    /// Maps a UI point to a hotbar slot, or `None` outside every slot.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        self.slots.iter().position(|slot| slot.contains(x, y))
    }
}

/// Icon shown in a survival hotbar or inventory slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotbarIcon {
    /// A block item drawn as an isometric cube from its top/side texture layers.
    Block {
        /// Top texture-array layer.
        top: u32,
        /// Side texture-array layer.
        side: u32,
    },
    /// The procedural apple icon.
    Apple,
}

/// Which primitive an apple icon part belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplePartKind {
    /// Red apple body.
    Body,
    /// Lighter highlight on the body.
    Highlight,
    /// Dark stem.
    Stem,
    /// Green leaf.
    Leaf,
}

/// One rectangle of the procedural apple icon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ApplePart {
    /// Drawable rectangle.
    pub rect: UiRect,
    /// Semantic colour group.
    pub kind: ApplePartKind,
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

/// Item name and hint shown in the survival inventory footer.
pub fn survival_item_info(registry: &Registry, stack: Option<ItemStack>) -> (String, String) {
    let Some(stack) = stack.filter(|stack| stack.count > 0) else {
        return ("Empty slot".to_string(), "No item".to_string());
    };
    let item = registry.item(stack.item);
    let name = display_name(item.id);
    let hint = if item.edible() {
        format!("Food +{} hunger", item.food.unwrap_or(0))
    } else if let Some(block) = item.block {
        hardness_hint(registry.block(block).hardness)
    } else {
        "No use".to_string()
    };
    (name, hint)
}

/// Computes the centred panel and the nine-slot survival hotbar row.
pub fn survival_layout(screen: [f32; 2]) -> SurvivalInventoryLayout {
    let width = screen[0].max(1.0);
    let height = screen[1].max(1.0);
    let gap = INVENTORY_GAP;
    let max_grid_width = (width - 2.0 * INVENTORY_MARGIN - 2.0 * SURVIVAL_PADDING).max(1.0);
    let columns = SURVIVAL_COLUMNS as f32;
    let slot_from_width = (max_grid_width - gap * (columns - 1.0)) / columns;
    let slot_size = slot_from_width.clamp(MIN_SLOT_SIZE, INVENTORY_SLOT_SIZE);
    let grid_width = columns * slot_size + gap * (columns - 1.0);
    let panel_width = grid_width + 2.0 * SURVIVAL_PADDING;
    let panel_height = INVENTORY_TITLE_HEIGHT + slot_size + INVENTORY_FOOTER_HEIGHT;
    let panel = UiRect::new(
        (width - panel_width) / 2.0,
        (height - panel_height) / 2.0,
        panel_width,
        panel_height,
    );
    let grid_x = panel.x + SURVIVAL_PADDING;
    let grid_y = panel.y + INVENTORY_TITLE_HEIGHT;
    let slots = (0..SURVIVAL_COLUMNS)
        .map(|index| UiRect::new(grid_x + index as f32 * (slot_size + gap), grid_y, slot_size, slot_size))
        .collect();
    SurvivalInventoryLayout {
        panel,
        slots,
        slot_size,
    }
}

/// Maps a UI point to a survival hotbar slot.
pub fn survival_hit_test(screen: [f32; 2], x: f32, y: f32) -> Option<usize> {
    survival_layout(screen).hit_test(x, y)
}

/// Moves the survival inventory selection by arrow-key steps, clamped to the row.
pub fn move_survival_selection(selected: usize, dx: i32, dy: i32, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let delta = i64::from(dx) + i64::from(dy);
    (selected as i64 + delta).clamp(0, count as i64 - 1) as usize
}

/// Cycles the survival inventory selection with the mouse wheel.
pub fn cycle_survival_selection(selected: usize, steps: i32, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    (selected as i32 + steps).rem_euclid(count as i32) as usize
}

/// Stack count text, mirroring Minecraft's rule of hiding one-item counts.
pub fn count_label(count: u8) -> Option<String> {
    (count > 1).then(|| count.to_string())
}

/// Icon for an inventory stack: block items use their cube icon and apples use
/// the procedural icon. Empty slots and unknown non-block items have no icon.
pub fn item_icon(registry: &Registry, stack: Option<ItemStack>) -> Option<HotbarIcon> {
    let stack = stack.filter(|stack| stack.count > 0)?;
    let item = registry.item(stack.item);
    if let Some(block) = item.block {
        let block = registry.block(block);
        return Some(HotbarIcon::Block {
            top: u32::from(block.textures.top.0),
            side: u32::from(block.textures.side.0),
        });
    }
    (item.id == "openblock:apple").then_some(HotbarIcon::Apple)
}

/// Block a right-click would place in the given mode.
///
/// Survival uses the selected stack's block item and therefore ignores the
/// creative palette; creative keeps the palette pick and ignores the inventory.
pub fn placement_target(
    mode: GameMode,
    palette: BlockId,
    registry: &Registry,
    selected: Option<ItemStack>,
) -> Option<BlockId> {
    match mode {
        GameMode::Creative => (palette != BlockId::AIR).then_some(palette),
        GameMode::Survival => selected
            .filter(|stack| stack.count > 0)
            .and_then(|stack| registry.item(stack.item).block),
    }
}

/// Whether right-click should eat the selected stack instead of placing it.
pub fn should_eat(registry: &Registry, selected: Option<ItemStack>, hunger: f32) -> bool {
    selected.is_some_and(|stack| {
        stack.count > 0 && hunger.is_finite() && hunger < MAX_HUNGER && registry.item(stack.item).edible()
    })
}

/// Procedural apple icon geometry, in draw order.
///
/// The apple is a stack of rectangles approximating a round red body, with a
/// lighter highlight, a dark stem and a green leaf. All parts stay inside the
/// `size` × `size` box at `(x, y)`.
pub fn apple_icon_parts(x: f32, y: f32, size: f32) -> [ApplePart; 8] {
    let unit = size.max(0.0) / 8.0;
    let part = |ux: f32, uy: f32, uw: f32, uh: f32, kind| ApplePart {
        rect: UiRect::new(x + ux * unit, y + uy * unit, uw * unit, uh * unit),
        kind,
    };
    [
        part(2.0, 2.0, 4.0, 1.0, ApplePartKind::Body),
        part(1.0, 3.0, 6.0, 1.0, ApplePartKind::Body),
        part(1.0, 4.0, 6.0, 1.0, ApplePartKind::Body),
        part(2.0, 5.0, 4.0, 1.0, ApplePartKind::Body),
        part(3.0, 6.0, 2.0, 1.0, ApplePartKind::Body),
        part(2.0, 3.0, 1.0, 1.0, ApplePartKind::Highlight),
        part(4.0, 1.0, 1.0, 2.0, ApplePartKind::Stem),
        part(5.0, 1.0, 2.0, 1.0, ApplePartKind::Leaf),
    ]
}

fn apple_color(kind: ApplePartKind) -> [f32; 4] {
    match kind {
        ApplePartKind::Body => [0.82, 0.14, 0.17, 1.0],
        ApplePartKind::Highlight => [1.0, 0.42, 0.42, 0.9],
        ApplePartKind::Stem => [0.28, 0.17, 0.08, 1.0],
        ApplePartKind::Leaf => [0.28, 0.64, 0.22, 1.0],
    }
}

/// Draws the procedural apple icon with [`UiFrame`] primitives.
pub fn draw_apple_icon(frame: &mut UiFrame, x: f32, y: f32, size: f32) {
    if !x.is_finite() || !y.is_finite() || !size.is_finite() || size <= 0.0 {
        return;
    }
    for part in apple_icon_parts(x, y, size) {
        frame.rect(
            part.rect.x,
            part.rect.y,
            part.rect.w,
            part.rect.h,
            apple_color(part.kind),
        );
    }
}

fn draw_item_icon(frame: &mut UiFrame, registry: &Registry, stack: Option<ItemStack>, x: f32, y: f32, size: f32) {
    match item_icon(registry, stack) {
        Some(HotbarIcon::Block { top, side }) => frame.block_icon(x, y, size, top, side),
        Some(HotbarIcon::Apple) => draw_apple_icon(frame, x, y, size),
        None => {}
    }
}

fn draw_stack_count(frame: &mut UiFrame, slot: UiRect, count: u8) {
    let Some(label) = count_label(count) else {
        return;
    };
    let scale = 1.0;
    let width = frame.text_width(scale, &label);
    let x = slot.x + slot.w - width - 5.0;
    let y = slot.y + slot.h - 12.0;
    frame.text(x + 1.0, y + 1.0, scale, [0.02, 0.02, 0.03, 0.9], &label);
    frame.text(x, y, scale, [1.0, 1.0, 1.0, 1.0], &label);
}

/// Draws the survival hotbar from the authoritative host inventory. Empty slots
/// show only a dim outline; counts are hidden for single items.
pub fn draw_survival_hotbar(
    frame: &mut UiFrame,
    ui: &UiState,
    registry: &Registry,
    inventory: &[Option<ItemStack>; HOTBAR_SLOTS],
    selected: usize,
    screen: [f32; 2],
) {
    for (index, slot) in ui.hotbar_rects(screen).iter().enumerate() {
        let stack = inventory[index];
        let active = index == selected;
        let empty = stack.is_none();
        if !empty {
            let background = if active {
                [0.24, 0.28, 0.34, 0.88]
            } else {
                [0.10, 0.11, 0.14, 0.72]
            };
            frame.rect(slot.x, slot.y, slot.w, slot.h, background);
        }
        let border = if active {
            [1.0, 0.92, 0.45, 1.0]
        } else if empty {
            [0.38, 0.41, 0.46, 0.55]
        } else {
            [0.62, 0.66, 0.72, 0.9]
        };
        frame.outline(slot.x, slot.y, slot.w, slot.h, if active { 2.0 } else { 1.0 }, border);
        if let Some(stack) = stack {
            draw_item_icon(
                frame,
                registry,
                Some(stack),
                slot.x + 6.0,
                slot.y + 5.0,
                (slot.w - 12.0).max(0.0),
            );
            draw_stack_count(frame, *slot, stack.count);
        }
    }
}

/// Draws the survival inventory screen: a single nine-slot hotbar row with item
/// names, hints and counts, plus a footer describing the hovered slot.
pub fn draw_survival(
    frame: &mut UiFrame,
    ui: &UiState,
    registry: &Registry,
    inventory: &[Option<ItemStack>; HOTBAR_SLOTS],
    selected: usize,
    screen: [f32; 2],
) {
    let layout = survival_layout(screen);
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
        "Inventory",
    );

    for (index, slot) in layout.slots.iter().enumerate() {
        let stack = inventory[index];
        let active = index == selected;
        let hovered = ui.inventory.hovered == Some(index);
        let background = if hovered {
            [0.25, 0.30, 0.38, 0.98]
        } else {
            [0.11, 0.13, 0.17, 0.94]
        };
        frame.rect(slot.x, slot.y, slot.w, slot.h, background);
        let (thickness, border) = if active {
            (2.0, [1.0, 0.92, 0.45, 1.0])
        } else if hovered {
            (2.0, [0.88, 0.92, 0.98, 1.0])
        } else if stack.is_none() {
            (1.0, [0.30, 0.33, 0.38, 0.75])
        } else {
            (1.0, [0.32, 0.36, 0.43, 0.95])
        };
        frame.outline(slot.x, slot.y, slot.w, slot.h, thickness, border);
        if let Some(stack) = stack {
            draw_item_icon(
                frame,
                registry,
                Some(stack),
                slot.x + 6.0,
                slot.y + 5.0,
                (slot.w - 12.0).max(0.0),
            );
            draw_stack_count(frame, *slot, stack.count);
        }
    }

    let footer_top = layout.panel.y + layout.panel.h - INVENTORY_FOOTER_HEIGHT;
    frame.rect(
        layout.panel.x + SURVIVAL_PADDING,
        footer_top,
        layout.panel.w - 2.0 * SURVIVAL_PADDING,
        1.0,
        [0.30, 0.34, 0.40, 0.9],
    );
    let info_slot = ui
        .inventory
        .hovered
        .filter(|index| *index < HOTBAR_SLOTS)
        .unwrap_or(selected.min(HOTBAR_SLOTS - 1));
    let (name, hint) = survival_item_info(registry, inventory[info_slot]);
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
        "Click/Enter select   Arrows/Wheel navigate   E/Esc close",
    );
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

    #[test]
    fn survival_layout_is_a_centred_nine_slot_row() {
        let screen = [1280.0, 720.0];
        let layout = survival_layout(screen);
        assert_eq!(layout.slots.len(), HOTBAR_SLOTS);
        assert_eq!(layout.slot_size, INVENTORY_SLOT_SIZE);
        let centre = layout.panel.center();
        assert!((centre.0 - 640.0).abs() < 0.01);
        assert!((centre.1 - 360.0).abs() < 0.01);
        for slot in &layout.slots {
            assert!(layout.panel.contains(slot.x + 0.1, slot.y + 0.1));
            assert!(layout.panel.contains(slot.x + slot.w - 0.1, slot.y + slot.h - 0.1));
        }
        assert_eq!(
            layout.hit_test(layout.slots[4].center().0, layout.slots[4].center().1),
            Some(4)
        );
        assert_eq!(survival_hit_test(screen, 1.0, 1.0), None);
    }

    #[test]
    fn survival_selection_navigation_clamps_and_wheel_wraps() {
        assert_eq!(move_survival_selection(0, 1, 0, HOTBAR_SLOTS), 1);
        assert_eq!(move_survival_selection(0, 0, 1, HOTBAR_SLOTS), 1);
        assert_eq!(move_survival_selection(0, -1, 0, HOTBAR_SLOTS), 0);
        assert_eq!(move_survival_selection(8, 1, 0, HOTBAR_SLOTS), 8);
        assert_eq!(move_survival_selection(8, 0, 2, HOTBAR_SLOTS), 8);
        assert_eq!(cycle_survival_selection(8, 1, HOTBAR_SLOTS), 0);
        assert_eq!(cycle_survival_selection(0, -1, HOTBAR_SLOTS), 8);
        assert_eq!(cycle_survival_selection(3, 20, HOTBAR_SLOTS), 5);
        assert_eq!(move_survival_selection(3, 1, 0, 0), 0);
        assert_eq!(cycle_survival_selection(3, 1, 0), 0);
    }

    #[test]
    fn survival_hotbar_counts_are_visible_only_above_one() {
        assert_eq!(count_label(0), None);
        assert_eq!(count_label(1), None);
        assert_eq!(count_label(2).as_deref(), Some("2"));
        assert_eq!(count_label(64).as_deref(), Some("64"));
    }

    #[test]
    fn survival_item_icons_use_blocks_and_the_procedural_apple() {
        let registry = Registry::base();
        let stone = registry.item_id("openblock:stone").expect("stone item");
        let apple = registry.item_id("openblock:apple").expect("apple item");
        let stone_icon = item_icon(&registry, Some(ItemStack::new(stone, 8))).expect("block icon");
        assert!(matches!(stone_icon, HotbarIcon::Block { .. }));
        assert_eq!(
            item_icon(&registry, Some(ItemStack::new(apple, 3))),
            Some(HotbarIcon::Apple)
        );
        assert_eq!(item_icon(&registry, None), None);
        assert_eq!(item_icon(&registry, Some(ItemStack::new(stone, 0))), None);
    }

    #[test]
    fn apple_icon_geometry_is_inside_its_box_and_has_the_expected_parts() {
        let parts = apple_icon_parts(20.0, 30.0, 24.0);
        assert_eq!(parts.len(), 8);
        let mut body = 0;
        let mut highlight = 0;
        let mut stem = 0;
        let mut leaf = 0;
        for part in &parts {
            assert!(part.rect.w > 0.0 && part.rect.h > 0.0);
            assert!(part.rect.x >= 20.0 && part.rect.y >= 30.0);
            assert!(part.rect.x + part.rect.w <= 44.0 + 1e-4);
            assert!(part.rect.y + part.rect.h <= 54.0 + 1e-4);
            match part.kind {
                ApplePartKind::Body => body += 1,
                ApplePartKind::Highlight => highlight += 1,
                ApplePartKind::Stem => stem += 1,
                ApplePartKind::Leaf => leaf += 1,
            }
        }
        assert_eq!((body, highlight, stem, leaf), (5, 1, 1, 1));
        let stem = parts
            .iter()
            .find(|part| part.kind == ApplePartKind::Stem)
            .expect("stem");
        assert!(stem.rect.x > 20.0 && stem.rect.x + stem.rect.w < 44.0);
        let leaf = parts
            .iter()
            .find(|part| part.kind == ApplePartKind::Leaf)
            .expect("leaf");
        assert!(leaf.rect.x > stem.rect.x, "leaf grows to the right of the stem");

        // Drawing never pushes geometry outside the frame.
        let mut frame = UiFrame::new(1280.0, 720.0);
        draw_apple_icon(&mut frame, 20.0, 30.0, 24.0);
        let (vertices, quads) = frame.measure();
        assert_eq!(quads, parts.len());
        assert_eq!(vertices, quads * 6);
    }

    #[test]
    fn placement_uses_selected_block_in_survival_and_palette_in_creative() {
        let registry = Registry::base();
        let stone = registry.item_id("openblock:stone").expect("stone item");
        let apple = registry.item_id("openblock:apple").expect("apple item");
        let stone_block = registry.id("openblock:stone").expect("stone block");
        let dirt_block = registry.id("openblock:dirt").expect("dirt block");
        let stack = Some(ItemStack::new(stone, 8));

        assert_eq!(
            placement_target(GameMode::Survival, dirt_block, &registry, stack),
            Some(stone_block),
            "survival must place the selected stack's block"
        );
        assert_eq!(
            placement_target(
                GameMode::Survival,
                dirt_block,
                &registry,
                Some(ItemStack::new(apple, 1))
            ),
            None,
            "an edible non-block item cannot be placed"
        );
        assert_eq!(placement_target(GameMode::Survival, dirt_block, &registry, None), None);
        assert_eq!(
            placement_target(GameMode::Creative, dirt_block, &registry, stack),
            Some(dirt_block),
            "creative ignores the inventory and keeps the palette pick"
        );
        assert_eq!(
            placement_target(GameMode::Creative, BlockId::AIR, &registry, stack),
            None
        );
    }

    #[test]
    fn eating_requires_edible_food_and_missing_hunger() {
        let registry = Registry::base();
        let apple = registry.item_id("openblock:apple").expect("apple item");
        let stone = registry.item_id("openblock:stone").expect("stone item");
        assert!(should_eat(&registry, Some(ItemStack::new(apple, 1)), 12.0));
        assert!(should_eat(&registry, Some(ItemStack::new(apple, 1)), 1.0));
        assert!(!should_eat(&registry, Some(ItemStack::new(apple, 1)), 20.0));
        assert!(!should_eat(&registry, Some(ItemStack::new(stone, 1)), 12.0));
        assert!(!should_eat(&registry, None, 12.0));
        assert!(!should_eat(&registry, Some(ItemStack::new(apple, 0)), 12.0));
        assert!(!should_eat(&registry, Some(ItemStack::new(apple, 1)), f32::NAN));
    }

    #[test]
    fn survival_item_info_reports_food_and_block_hints() {
        let registry = Registry::base();
        let apple = registry.item_id("openblock:apple").expect("apple item");
        let stone = registry.item_id("openblock:stone").expect("stone item");
        let (name, hint) = survival_item_info(&registry, Some(ItemStack::new(apple, 3)));
        assert_eq!(name, "Apple");
        assert_eq!(hint, "Food +6 hunger");
        let (name, hint) = survival_item_info(&registry, Some(ItemStack::new(stone, 8)));
        assert_eq!(name, "Stone");
        assert!(hint.starts_with("hardness "));
        assert_eq!(
            survival_item_info(&registry, None),
            ("Empty slot".to_string(), "No item".to_string())
        );
    }
}

//! Furnace and chest modal screens: layout, click-to-cursor transfer rules and
//! drawing.
//!
//! The client never writes container slots directly. [`ContainerSession::click`]
//! is a pure decision function over a read-only [`ContainerSnapshot`]; it
//! returns the [`ContainerAction`] the game loop must send to the authoritative
//! host through the `put_*` / `take_*` slot APIs, plus the cursor stack that
//! results when the host accepts it. The same model drives mouse clicks,
//! keyboard activation and the offscreen demo screenshots.
//!
//! The hotbar row is part of both screens. Taking a stack out of the hotbar is
//! an inventory mutation the interim host does not expose as a single call; the
//! client routes it through the selected-slot `consume_selected` API (in
//! survival only) and restores the previous selection. Putting a cursor stack
//! back uses `give_item`, so the transfer is intentionally conservative for
//! damaged tools and armor: those are dropped as item entities instead of being
//! silently repaired by the count-based give API.

use ob_content::Registry;
use ob_math::BlockPos;
use ob_render::UiFrame;
use ob_server::{CHEST_SLOTS, ContainerState, FurnaceSlot, FurnaceState, ItemStack};

use crate::inventory;
use crate::ui_state::{HOTBAR_SLOTS, UiRect};

/// Width of the furnace and chest modal panels.
pub const CONTAINER_PANEL_WIDTH: f32 = 620.0;
/// Height of the furnace modal panel.
pub const FURNACE_PANEL_HEIGHT: f32 = 560.0;
/// Height of the chest modal panel.
pub const CHEST_PANEL_HEIGHT: f32 = 480.0;
/// Inner horizontal and vertical panel margin.
pub const CONTAINER_MARGIN: f32 = 28.0;
/// Edge length of a container slot at 1080p.
pub const CONTAINER_SLOT_SIZE: f32 = 48.0;
/// Gap between two container slots.
pub const CONTAINER_SLOT_GAP: f32 = 8.0;
/// Gap between the container grid and the player hotbar row.
pub const HOTBAR_GAP: f32 = 28.0;
/// Height reserved for the screen title and subtitle.
pub const CONTAINER_TITLE_HEIGHT: f32 = 68.0;
/// Height reserved for key hints under the hotbar row.
pub const CONTAINER_FOOTER_HEIGHT: f32 = 54.0;
/// Width of the cook-progress bar inside the furnace arrow.
pub const COOK_PROGRESS_WIDTH: f32 = 92.0;
/// Height of the cook-progress bar inside the furnace arrow.
pub const COOK_PROGRESS_HEIGHT: f32 = 18.0;
/// Width of the furnace flame indicator.
pub const FLAME_WIDTH: f32 = 26.0;
/// Height of the furnace flame indicator.
pub const FLAME_HEIGHT: f32 = 32.0;

/// Which authoritative block entity a modal screen is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    /// A furnace with input, fuel and output slots.
    Furnace,
    /// A chest with 27 storage slots.
    Chest,
}

impl ContainerKind {
    /// Modal screen title.
    pub fn title(self) -> &'static str {
        match self {
            ContainerKind::Furnace => "Furnace",
            ContainerKind::Chest => "Chest",
        }
    }
}

/// One addressable slot of a container screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotId {
    /// Furnace input slot (smeltable item).
    FurnaceInput,
    /// Furnace fuel slot.
    FurnaceFuel,
    /// Furnace output slot (take-only).
    FurnaceOutput,
    /// One of the 27 chest slots.
    Chest(usize),
    /// One of the nine player hotbar slots.
    Hotbar(usize),
}

impl SlotId {
    /// The corresponding furnace slot, if this is a furnace slot.
    pub fn furnace_slot(self) -> Option<FurnaceSlot> {
        match self {
            SlotId::FurnaceInput => Some(FurnaceSlot::Input),
            SlotId::FurnaceFuel => Some(FurnaceSlot::Fuel),
            SlotId::FurnaceOutput => Some(FurnaceSlot::Output),
            SlotId::Chest(_) | SlotId::Hotbar(_) => None,
        }
    }
}

/// One slot rectangle in a container layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContainerSlot {
    /// What the rectangle addresses.
    pub id: SlotId,
    /// Drawable/hit-testable rectangle.
    pub rect: UiRect,
}

/// Computed geometry of a furnace or chest screen.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerLayout {
    /// Centred modal panel.
    pub panel: UiRect,
    /// Every addressable slot, in drawing order.
    pub slots: Vec<ContainerSlot>,
    /// Y of the footer separator.
    pub footer_top: f32,
    /// Cook-progress bar rectangle (furnace only).
    pub cook_bar: Option<UiRect>,
    /// Flame indicator rectangle (furnace only).
    pub flame: Option<UiRect>,
}

impl ContainerLayout {
    /// Maps a UI point to a slot address.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<SlotId> {
        self.slots
            .iter()
            .find(|slot| slot.rect.contains(x, y))
            .map(|slot| slot.id)
    }

    /// Rectangle of one slot address.
    pub fn slot_rect(&self, id: SlotId) -> Option<UiRect> {
        self.slots.iter().find(|slot| slot.id == id).map(|slot| slot.rect)
    }

    /// The hotbar slot rectangles in slot order.
    pub fn hotbar_rects(&self) -> Vec<UiRect> {
        self.slots
            .iter()
            .filter_map(|slot| match slot.id {
                SlotId::Hotbar(_) => Some(slot.rect),
                _ => None,
            })
            .collect()
    }
}

/// Centred panel rectangle with responsive slot sizing for a screen.
///
/// `grid_rows` is the number of slot rows above the hotbar row (2 for the
/// furnace's input/fuel arrangement, 3 for the chest). Both width and height
/// are considered so the panel remains usable on small windows.
fn container_panel(screen: [f32; 2], height: f32, grid_rows: f32) -> (UiRect, f32) {
    let width = screen[0].max(1.0);
    let height_px = screen[1].max(1.0);
    let max_width = (width - 2.0 * CONTAINER_MARGIN).max(1.0);
    let panel_width = CONTAINER_PANEL_WIDTH.min(max_width);
    let panel_height = height.min(height_px - 2.0 * CONTAINER_MARGIN).max(1.0);
    let rows = grid_rows + 1.0;
    let vertical_room = (panel_height - CONTAINER_TITLE_HEIGHT - CONTAINER_FOOTER_HEIGHT - 64.0).max(1.0);
    let height_fit = (vertical_room - (rows - 1.0) * CONTAINER_SLOT_GAP) / rows;
    let slot_size = CONTAINER_SLOT_SIZE
        .min((panel_width - 2.0 * CONTAINER_MARGIN - 8.0 * CONTAINER_SLOT_GAP) / 9.0)
        .min(height_fit)
        .max(16.0);
    (
        UiRect::new(
            (width - panel_width) / 2.0,
            (height_px - panel_height) / 2.0,
            panel_width,
            panel_height,
        ),
        slot_size,
    )
}

/// Computes the nine-slot hotbar row shared by both container screens.
fn container_hotbar(panel: UiRect, slot_size: f32) -> Vec<ContainerSlot> {
    let gap = CONTAINER_SLOT_GAP;
    let row_width = 9.0 * slot_size + 8.0 * gap;
    let x = panel.x + (panel.w - row_width) / 2.0;
    let y = panel.y + panel.h - CONTAINER_FOOTER_HEIGHT - slot_size;
    (0..HOTBAR_SLOTS)
        .map(|index| ContainerSlot {
            id: SlotId::Hotbar(index),
            rect: UiRect::new(x + index as f32 * (slot_size + gap), y, slot_size, slot_size),
        })
        .collect()
}

/// Computes the furnace screen layout: input/fuel on the left, output on the
/// right, cook arrow between them, flame below the arrow and the hotbar row.
pub fn furnace_layout(screen: [f32; 2]) -> ContainerLayout {
    let (panel, slot_size) = container_panel(screen, FURNACE_PANEL_HEIGHT, 2.0);
    let margin = CONTAINER_MARGIN;
    let input_x = panel.x + margin + 30.0;
    let input_y = panel.y + CONTAINER_TITLE_HEIGHT + slot_size * 0.6;
    let output_x = panel.x + panel.w - margin - slot_size - 70.0;
    let output_y = input_y + slot_size * 0.5;
    let fuel_y = input_y + slot_size + slot_size * 0.7;
    let arrow_x = input_x + slot_size + 38.0;
    let arrow_y = input_y + slot_size * 0.5 - COOK_PROGRESS_HEIGHT / 2.0;
    let arrow_width = (output_x - arrow_x - 28.0).max(COOK_PROGRESS_WIDTH);

    let mut slots = vec![
        ContainerSlot {
            id: SlotId::FurnaceInput,
            rect: UiRect::new(input_x, input_y, slot_size, slot_size),
        },
        ContainerSlot {
            id: SlotId::FurnaceFuel,
            rect: UiRect::new(input_x, fuel_y, slot_size, slot_size),
        },
        ContainerSlot {
            id: SlotId::FurnaceOutput,
            rect: UiRect::new(output_x, output_y, slot_size, slot_size),
        },
    ];
    slots.extend(container_hotbar(panel, slot_size));
    ContainerLayout {
        panel,
        slots,
        footer_top: panel.y + panel.h - CONTAINER_FOOTER_HEIGHT,
        cook_bar: Some(UiRect::new(arrow_x, arrow_y, arrow_width, COOK_PROGRESS_HEIGHT)),
        flame: Some(UiRect::new(
            arrow_x + arrow_width / 2.0 - FLAME_WIDTH / 2.0,
            arrow_y + COOK_PROGRESS_HEIGHT + 20.0,
            FLAME_WIDTH,
            FLAME_HEIGHT,
        )),
    }
}

/// Computes the chest screen layout: a 3x9 storage grid above the hotbar row.
pub fn chest_layout(screen: [f32; 2]) -> ContainerLayout {
    let (panel, slot_size) = container_panel(screen, CHEST_PANEL_HEIGHT, 3.0);
    let gap = CONTAINER_SLOT_GAP;
    let grid_width = 9.0 * slot_size + 8.0 * gap;
    let grid_x = panel.x + (panel.w - grid_width) / 2.0;
    let grid_y = panel.y + CONTAINER_TITLE_HEIGHT + slot_size * 0.4;
    let mut slots = Vec::with_capacity(CHEST_SLOTS + HOTBAR_SLOTS);
    for index in 0..CHEST_SLOTS {
        let row = index / 9;
        let column = index % 9;
        slots.push(ContainerSlot {
            id: SlotId::Chest(index),
            rect: UiRect::new(
                grid_x + column as f32 * (slot_size + gap),
                grid_y + row as f32 * (slot_size + gap),
                slot_size,
                slot_size,
            ),
        });
    }
    slots.extend(container_hotbar(panel, slot_size));
    ContainerLayout {
        panel,
        slots,
        footer_top: panel.y + panel.h - CONTAINER_FOOTER_HEIGHT,
        cook_bar: None,
        flame: None,
    }
}

/// Builds the layout for one container kind.
pub fn container_layout(kind: ContainerKind, screen: [f32; 2]) -> ContainerLayout {
    match kind {
        ContainerKind::Furnace => furnace_layout(screen),
        ContainerKind::Chest => chest_layout(screen),
    }
}

/// Read-only snapshot of every stack a container click can see.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSnapshot {
    /// Furnace state, if a furnace is open.
    pub furnace: Option<FurnaceState>,
    /// Chest slots, if a chest is open.
    pub chest: [Option<ItemStack>; CHEST_SLOTS],
    /// Player hotbar.
    pub inventory: [Option<ItemStack>; HOTBAR_SLOTS],
}

impl Default for ContainerSnapshot {
    fn default() -> Self {
        Self {
            furnace: None,
            chest: [None; CHEST_SLOTS],
            inventory: [None; HOTBAR_SLOTS],
        }
    }
}

impl ContainerSnapshot {
    /// Contents of one screen slot.
    pub fn stack(&self, id: SlotId) -> Option<ItemStack> {
        match id {
            SlotId::FurnaceInput => self.furnace.as_ref().and_then(|state| state.input),
            SlotId::FurnaceFuel => self.furnace.as_ref().and_then(|state| state.fuel),
            SlotId::FurnaceOutput => self.furnace.as_ref().and_then(|state| state.output),
            SlotId::Chest(index) => self.chest.get(index).copied().flatten(),
            SlotId::Hotbar(index) => self.inventory.get(index).copied().flatten(),
        }
    }
}

/// Mouse button used for a container click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickButton {
    /// Pick the whole stack, place the whole cursor stack, swap, or take output.
    Left,
    /// Pick half a stack, place one item, or merge output into the cursor.
    Right,
}

/// Authoritative host operation requested by one click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerAction {
    /// Take `count` items out of a furnace slot.
    TakeFurnace {
        /// Open furnace position.
        pos: BlockPos,
        /// Address of the slot.
        slot: FurnaceSlot,
        /// Items to remove.
        count: u8,
    },
    /// Replace a furnace input/fuel slot with `stack`.
    PutFurnace {
        /// Open furnace position.
        pos: BlockPos,
        /// Address of the slot (never the output slot).
        slot: FurnaceSlot,
        /// New slot contents.
        stack: ItemStack,
    },
    /// Take `count` items out of a chest slot.
    TakeChest {
        /// Open chest position.
        pos: BlockPos,
        /// Chest slot index.
        slot: usize,
        /// Items to remove.
        count: u8,
    },
    /// Replace one chest slot with `stack`.
    PutChest {
        /// Open chest position.
        pos: BlockPos,
        /// Chest slot index.
        slot: usize,
        /// New slot contents.
        stack: ItemStack,
    },
    /// Take `count` items out of a hotbar slot (survival only).
    TakeHotbar {
        /// Hotbar slot index.
        slot: usize,
        /// Items to remove.
        count: u8,
    },
    /// Add a cursor stack through the count-based inventory API.
    GiveInventory {
        /// Stack to add.
        stack: ItemStack,
    },
}

/// Result of one pure container click.
#[derive(Debug, Clone, PartialEq)]
pub struct ClickOutcome {
    /// Host operation to apply, when the click is accepted.
    pub action: Option<ContainerAction>,
    /// Cursor stack after the host accepts [`ClickOutcome::action`].
    pub cursor_after: Option<ItemStack>,
    /// User-facing refusal or hint, when no operation is requested.
    pub message: Option<String>,
}

impl ClickOutcome {
    /// A refused click with an explanatory message.
    fn refused(message: impl Into<String>) -> Self {
        Self {
            action: None,
            cursor_after: None,
            message: Some(message.into()),
        }
    }
}

/// One open container screen: which block entity is open, the cursor stack and
/// the keyboard-selected slot.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSession {
    /// Screen kind.
    pub kind: ContainerKind,
    /// Block entity position.
    pub pos: BlockPos,
    /// Stack held by the cursor, if any.
    pub cursor: Option<ItemStack>,
    /// Keyboard-selected slot.
    pub selected: SlotId,
}

impl ContainerSession {
    /// Creates a session with a sensible initial keyboard focus.
    pub fn new(kind: ContainerKind, pos: BlockPos) -> Self {
        Self {
            kind,
            pos,
            cursor: None,
            selected: match kind {
                ContainerKind::Furnace => SlotId::FurnaceInput,
                ContainerKind::Chest => SlotId::Chest(0),
            },
        }
    }

    /// Builds the layout for the open screen.
    pub fn layout(&self, screen: [f32; 2]) -> ContainerLayout {
        container_layout(self.kind, screen)
    }

    /// Whether `slot` belongs to this container kind.
    pub fn accepts_slot(&self, slot: SlotId) -> bool {
        match self.kind {
            ContainerKind::Furnace => slot.furnace_slot().is_some() || matches!(slot, SlotId::Hotbar(_)),
            ContainerKind::Chest => matches!(slot, SlotId::Chest(_) | SlotId::Hotbar(_)),
        }
    }

    /// Decides what one click means without touching host state.
    ///
    /// `view` must describe the state the player currently sees. The returned
    /// action is what the game loop sends to the host; `cursor_after` is only
    /// installed after the host accepts the action.
    pub fn click(
        &self,
        registry: &Registry,
        view: &ContainerSnapshot,
        slot: SlotId,
        button: ClickButton,
    ) -> ClickOutcome {
        if !self.accepts_slot(slot) {
            return ClickOutcome::refused("That slot belongs to another container");
        }
        match self.cursor {
            None => self.pick(view, slot, button),
            Some(cursor) => self.place(registry, view, slot, button, cursor),
        }
    }

    /// Cursor-empty click: take a container or hotbar stack.
    fn pick(&self, view: &ContainerSnapshot, slot: SlotId, button: ClickButton) -> ClickOutcome {
        let Some(mut source) = view.stack(slot) else {
            return ClickOutcome {
                action: None,
                cursor_after: None,
                message: None,
            };
        };
        if source.count == 0 {
            return ClickOutcome::refused("That slot is empty");
        }
        let half = button == ClickButton::Right;
        match slot {
            SlotId::Hotbar(index) => {
                let count = if half { source.count.div_ceil(2) } else { source.count };
                source.count = count;
                ClickOutcome {
                    action: Some(ContainerAction::TakeHotbar { slot: index, count }),
                    cursor_after: Some(source),
                    message: None,
                }
            }
            SlotId::FurnaceOutput => {
                // The output slot is take-only; taking it always empties it.
                ClickOutcome {
                    action: Some(ContainerAction::TakeFurnace {
                        pos: self.pos,
                        slot: FurnaceSlot::Output,
                        count: source.count,
                    }),
                    cursor_after: Some(source),
                    message: None,
                }
            }
            SlotId::FurnaceInput | SlotId::FurnaceFuel => {
                let furnace_slot = slot.furnace_slot().expect("furnace slot");
                let count = if half { source.count.div_ceil(2) } else { source.count };
                source.count = count;
                ClickOutcome {
                    action: Some(ContainerAction::TakeFurnace {
                        pos: self.pos,
                        slot: furnace_slot,
                        count,
                    }),
                    cursor_after: Some(source),
                    message: None,
                }
            }
            SlotId::Chest(index) => {
                let count = if half { source.count.div_ceil(2) } else { source.count };
                source.count = count;
                ClickOutcome {
                    action: Some(ContainerAction::TakeChest {
                        pos: self.pos,
                        slot: index,
                        count,
                    }),
                    cursor_after: Some(source),
                    message: None,
                }
            }
        }
    }

    /// Cursor-filled click: place, merge or swap.
    fn place(
        &self,
        registry: &Registry,
        view: &ContainerSnapshot,
        slot: SlotId,
        button: ClickButton,
        cursor: ItemStack,
    ) -> ClickOutcome {
        if cursor.count == 0 {
            return ClickOutcome {
                action: None,
                cursor_after: None,
                message: None,
            };
        }
        let one = button == ClickButton::Right;
        match slot {
            SlotId::Hotbar(_) => {
                let count = if one { 1 } else { cursor.count };
                let mut given = cursor;
                given.count = count;
                ClickOutcome {
                    action: Some(ContainerAction::GiveInventory { stack: given }),
                    cursor_after: stack_remainder(cursor, count),
                    message: None,
                }
            }
            SlotId::FurnaceOutput => {
                let Some(output) = view.furnace.as_ref().and_then(|state| state.output) else {
                    return ClickOutcome::refused("Output slot is take-only");
                };
                if !stacks_merge(cursor, output) {
                    return ClickOutcome::refused("Output slot is take-only");
                }
                let max = max_stack(registry, cursor.item);
                let room = max.saturating_sub(cursor.count);
                if output.count > room {
                    return ClickOutcome::refused("Not enough room for the finished item");
                }
                let mut merged = cursor;
                merged.count += output.count;
                ClickOutcome {
                    action: Some(ContainerAction::TakeFurnace {
                        pos: self.pos,
                        slot: FurnaceSlot::Output,
                        count: output.count,
                    }),
                    cursor_after: Some(merged),
                    message: None,
                }
            }
            SlotId::FurnaceInput | SlotId::FurnaceFuel => {
                let furnace_slot = slot.furnace_slot().expect("furnace slot");
                let target = view.stack(slot);
                if one && target.is_some_and(|target| !stacks_merge(target, cursor)) {
                    return ClickOutcome::refused("Right-click only merges matching items; left-click swaps");
                }
                let Some((stack, cursor_after, message)) = plan_place(registry, target, cursor, one) else {
                    return ClickOutcome::refused("That slot cannot accept the item");
                };
                ClickOutcome {
                    action: Some(ContainerAction::PutFurnace {
                        pos: self.pos,
                        slot: furnace_slot,
                        stack,
                    }),
                    cursor_after,
                    message,
                }
            }
            SlotId::Chest(index) => {
                let target = view.stack(slot);
                if one && target.is_some_and(|target| !stacks_merge(target, cursor)) {
                    return ClickOutcome::refused("Right-click only merges matching items; left-click swaps");
                }
                let Some((stack, cursor_after, message)) = plan_place(registry, target, cursor, one) else {
                    return ClickOutcome::refused("That slot cannot accept the item");
                };
                ClickOutcome {
                    action: Some(ContainerAction::PutChest {
                        pos: self.pos,
                        slot: index,
                        stack,
                    }),
                    cursor_after,
                    message,
                }
            }
        }
    }
}

/// Plans a merge/swap/single placement into a writable container slot.
///
/// Returns the new target stack, the cursor after the host applies it and an
/// optional informational message (for a partially merged stack).
fn plan_place(
    registry: &Registry,
    target: Option<ItemStack>,
    cursor: ItemStack,
    one: bool,
) -> Option<(ItemStack, Option<ItemStack>, Option<String>)> {
    match target {
        Some(target) if stacks_merge(target, cursor) => {
            let max = max_stack(registry, target.item);
            if target.count >= max {
                return None;
            }
            let wanted = if one { 1 } else { cursor.count };
            let moved = wanted.min(max - target.count);
            let mut merged = target;
            merged.count += moved;
            Some((merged, stack_remainder(cursor, moved), None))
        }
        Some(target) => {
            // Different item: swap the whole stack on left click, or put one
            // item and return the old stack on right click.
            if one {
                let mut placed = cursor;
                placed.count = 1;
                Some((placed, stack_remainder(cursor, 1), None))
            } else {
                Some((cursor, Some(target), None))
            }
        }
        None => {
            let count = if one { 1 } else { cursor.count };
            let mut placed = cursor;
            placed.count = count;
            Some((placed, stack_remainder(cursor, count), None))
        }
    }
}

/// Whether two stacks can merge (same item and durability).
fn stacks_merge(a: ItemStack, b: ItemStack) -> bool {
    a.item == b.item && a.durability == b.durability
}

/// Content-defined maximum stack size for an item id.
fn max_stack(registry: &Registry, item: ob_content::ItemId) -> u8 {
    if item.0 as usize >= registry.item_len() {
        return 1;
    }
    registry.item(item).max_stack.clamp(1, 64)
}

/// The cursor stack after removing `count` items.
fn stack_remainder(mut cursor: ItemStack, count: u8) -> Option<ItemStack> {
    if count >= cursor.count {
        None
    } else {
        cursor.count -= count;
        Some(cursor)
    }
}

/// Moves keyboard focus to the closest slot in the arrow-key direction.
pub fn move_selection(layout: &ContainerLayout, current: SlotId, dx: i32, dy: i32) -> SlotId {
    let Some(current_rect) = layout.slot_rect(current) else {
        return layout.slots.first().map_or(current, |slot| slot.id);
    };
    let (cx, cy) = current_rect.center();
    let direction = (dx as f32, dy as f32);
    let mut best: Option<(f32, SlotId)> = None;
    for slot in &layout.slots {
        if slot.id == current {
            continue;
        }
        let (sx, sy) = slot.rect.center();
        let delta = (sx - cx, sy - cy);
        let forward = delta.0 * direction.0 + delta.1 * direction.1;
        if forward <= 0.1 {
            continue;
        }
        let side = (delta.0 * direction.1 - delta.1 * direction.0).abs();
        let score = forward + side * 2.0;
        if best.is_none_or(|(best_score, _)| score < best_score) {
            best = Some((score, slot.id));
        }
    }
    best.map_or(current, |(_, id)| id)
}

/// Draws one container slot with its item, count and durability bar.
fn draw_slot(
    frame: &mut UiFrame,
    rect: UiRect,
    stack: Option<ItemStack>,
    registry: &Registry,
    hovered: bool,
    selected: bool,
    read_only: bool,
) {
    let background = if hovered {
        [0.25, 0.30, 0.38, 0.98]
    } else if selected {
        [0.18, 0.20, 0.26, 0.96]
    } else {
        [0.11, 0.13, 0.17, 0.94]
    };
    frame.rect(rect.x, rect.y, rect.w, rect.h, background);
    let (thickness, border) = if hovered {
        (2.0, [0.88, 0.92, 0.98, 1.0])
    } else if selected {
        (2.0, [1.0, 0.92, 0.45, 1.0])
    } else if read_only {
        (1.0, [0.55, 0.46, 0.30, 0.95])
    } else {
        (1.0, [0.32, 0.36, 0.43, 0.95])
    };
    frame.outline(rect.x, rect.y, rect.w, rect.h, thickness, border);
    inventory::draw_slot_stack(frame, rect, registry, stack);
}

/// Draws the modal chrome, title and slot grid shared by both screens.
fn draw_container_base(
    frame: &mut UiFrame,
    session: &ContainerSession,
    registry: &Registry,
    layout: &ContainerLayout,
    inventory_stacks: &[Option<ItemStack>; HOTBAR_SLOTS],
    cursor: (f32, f32),
    screen: [f32; 2],
) -> Option<SlotId> {
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
        layout.panel.y + 18.0,
        2.5,
        [1.0, 1.0, 1.0, 1.0],
        session.kind.title(),
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        layout.panel.y + 48.0,
        1.0,
        [0.70, 0.76, 0.84, 1.0],
        &format!("at {} {} {}", session.pos.x, session.pos.y, session.pos.z),
    );
    let hovered = layout.hit_test(cursor.0, cursor.1);
    for slot in &layout.slots {
        let stack = match slot.id {
            SlotId::Hotbar(index) => inventory_stacks.get(index).copied().flatten(),
            _ => None,
        };
        let read_only = slot.id == SlotId::FurnaceOutput;
        draw_slot(
            frame,
            slot.rect,
            stack,
            registry,
            hovered == Some(slot.id),
            session.selected == slot.id,
            read_only,
        );
    }
    hovered
}

/// Draws the item name/hint footer line shared by both screens.
fn draw_container_footer(
    frame: &mut UiFrame,
    layout: &ContainerLayout,
    registry: &Registry,
    view: &ContainerSnapshot,
    hovered: Option<SlotId>,
) {
    frame.rect(
        layout.panel.x + CONTAINER_MARGIN,
        layout.footer_top,
        layout.panel.w - 2.0 * CONTAINER_MARGIN,
        1.0,
        [0.30, 0.34, 0.40, 0.9],
    );
    let stack = hovered.and_then(|slot| view.stack(slot));
    let (name, hint) = inventory::survival_item_info(registry, stack);
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        layout.footer_top + 9.0,
        1.8,
        [1.0, 1.0, 1.0, 1.0],
        &name,
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        layout.footer_top + 27.0,
        1.0,
        [0.74, 0.80, 0.88, 1.0],
        &hint,
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        layout.panel.y + layout.panel.h - 14.0,
        1.0,
        [0.68, 0.74, 0.82, 1.0],
        "LMB take/place  RMB half/one  Arrows select  Enter use  E/Esc close",
    );
}

/// Draws the stack attached to the mouse cursor above the panel.
fn draw_cursor_stack(frame: &mut UiFrame, registry: &Registry, cursor_stack: Option<ItemStack>, cursor: (f32, f32)) {
    let Some(stack) = cursor_stack else {
        return;
    };
    let size = 34.0;
    let rect = UiRect::new(cursor.0 + 8.0, cursor.1 + 8.0, size, size);
    frame.rect(
        rect.x - 2.0,
        rect.y - 2.0,
        rect.w + 4.0,
        rect.h + 4.0,
        [0.02, 0.03, 0.05, 0.85],
    );
    inventory::draw_slot_stack(frame, rect, registry, Some(stack));
}

/// Draws the furnace screen.
pub fn draw_furnace(
    frame: &mut UiFrame,
    registry: &Registry,
    session: &ContainerSession,
    state: Option<&FurnaceState>,
    view: &ContainerSnapshot,
    cursor: (f32, f32),
    screen: [f32; 2],
) {
    let layout = furnace_layout(screen);
    let hovered = draw_container_base(frame, session, registry, &layout, &view.inventory, cursor, screen);
    let labels = [
        (SlotId::FurnaceInput, "Input"),
        (SlotId::FurnaceFuel, "Fuel"),
        (SlotId::FurnaceOutput, "Output"),
    ];
    for (id, label) in labels {
        if let Some(rect) = layout.slot_rect(id) {
            frame.text_centered(rect.center().0, rect.y - 16.0, 1.0, [0.80, 0.86, 0.94, 1.0], label);
        }
    }
    let state = state.cloned().unwrap_or_default();
    // Furnace-owned stacks.
    for slot in &layout.slots {
        let stack = match slot.id {
            SlotId::FurnaceInput => state.input,
            SlotId::FurnaceFuel => state.fuel,
            SlotId::FurnaceOutput => state.output,
            SlotId::Chest(_) | SlotId::Hotbar(_) => None,
        };
        if stack.is_some() {
            inventory::draw_slot_stack(frame, slot.rect, registry, stack);
        }
    }
    if let Some(arrow) = layout.cook_bar {
        let progress = if ob_content::SMELT_TICKS_PER_ITEM == 0 {
            0.0
        } else {
            (state.cook_progress as f32 / f32::from(ob_content::SMELT_TICKS_PER_ITEM)).clamp(0.0, 1.0)
        };
        frame.rect(
            arrow.x,
            arrow.y + arrow.h / 2.0 - 3.0,
            arrow.w,
            6.0,
            [0.16, 0.17, 0.21, 1.0],
        );
        frame.rect(
            arrow.x,
            arrow.y + arrow.h / 2.0 - 3.0,
            arrow.w * progress,
            6.0,
            [0.95, 0.72, 0.25, 1.0],
        );
        // Right-pointing head made of three stacked rectangles.
        frame.rect(
            arrow.x + arrow.w,
            arrow.y + arrow.h / 2.0 - 9.0,
            8.0,
            18.0,
            [0.55, 0.58, 0.64, 1.0],
        );
        frame.rect(
            arrow.x + arrow.w + 8.0,
            arrow.y + arrow.h / 2.0 - 5.0,
            6.0,
            10.0,
            [0.55, 0.58, 0.64, 1.0],
        );
        frame.text_centered(
            arrow.center().0,
            arrow.y - 18.0,
            1.0,
            [0.86, 0.90, 0.96, 1.0],
            &format!("Cook {:.0}%", progress * 100.0),
        );
    }
    if let Some(flame) = layout.flame {
        let burn = if state.burn_total == 0 {
            0.0
        } else {
            (state.burn_remaining as f32 / f32::from(state.burn_total)).clamp(0.0, 1.0)
        };
        frame.rect(flame.x, flame.y, flame.w, flame.h, [0.12, 0.09, 0.08, 1.0]);
        let fill_top = flame.y + flame.h * (1.0 - burn);
        if burn > 0.0 {
            frame.rect(
                flame.x + 2.0,
                fill_top,
                flame.w - 4.0,
                flame.h * burn,
                [0.94, 0.46, 0.15, 1.0],
            );
            frame.rect(
                flame.x + 6.0,
                fill_top + 3.0,
                flame.w - 12.0,
                (flame.h * burn - 6.0).max(0.0),
                [0.99, 0.78, 0.28, 1.0],
            );
        }
        let label = if state.is_lit() {
            format!("Lit {:.1}s", f32::from(state.burn_remaining) / 20.0)
        } else {
            "Unlit".to_string()
        };
        frame.text_centered(
            flame.center().0,
            flame.y + flame.h + 4.0,
            1.0,
            [0.86, 0.90, 0.96, 1.0],
            &label,
        );
    }
    draw_container_footer(frame, &layout, registry, view, hovered);
    draw_cursor_stack(frame, registry, session.cursor, cursor);
}

/// Draws the chest screen.
pub fn draw_chest(
    frame: &mut UiFrame,
    registry: &Registry,
    session: &ContainerSession,
    state: Option<&ContainerState>,
    view: &ContainerSnapshot,
    cursor: (f32, f32),
    screen: [f32; 2],
) {
    let layout = chest_layout(screen);
    let hovered = draw_container_base(frame, session, registry, &layout, &view.inventory, cursor, screen);
    if let Some(state) = state {
        for slot in &layout.slots {
            if let SlotId::Chest(index) = slot.id
                && let Some(stack) = state.slots.get(index).copied().flatten()
            {
                inventory::draw_slot_stack(frame, slot.rect, registry, Some(stack));
            }
        }
    }
    draw_container_footer(frame, &layout, registry, view, hovered);
    draw_cursor_stack(frame, registry, session.cursor, cursor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_content::{ItemId, Registry};

    fn registry() -> Registry {
        Registry::base()
    }

    fn stack(registry: &Registry, name: &str, count: u8) -> ItemStack {
        let item = registry.item_id(name).expect("item");
        ItemStack::new(item, count)
    }

    fn furnace_session() -> ContainerSession {
        ContainerSession::new(ContainerKind::Furnace, BlockPos::new(2, 64, -3))
    }

    fn chest_session() -> ContainerSession {
        ContainerSession::new(ContainerKind::Chest, BlockPos::new(-4, 70, 8))
    }

    fn furnace_view(input: Option<ItemStack>, fuel: Option<ItemStack>, output: Option<ItemStack>) -> ContainerSnapshot {
        ContainerSnapshot {
            furnace: Some(FurnaceState {
                input,
                fuel,
                output,
                burn_remaining: 400,
                burn_total: 1600,
                cook_progress: 120,
            }),
            ..ContainerSnapshot::default()
        }
    }

    #[test]
    fn furnace_layout_places_slots_inside_a_620_by_560_panel() {
        let screen = [1280.0, 720.0];
        let layout = furnace_layout(screen);
        assert_eq!(layout.slots.len(), 3 + HOTBAR_SLOTS);
        assert_eq!(layout.panel.w, CONTAINER_PANEL_WIDTH);
        assert_eq!(layout.panel.h, FURNACE_PANEL_HEIGHT);
        let centre = layout.panel.center();
        assert!((centre.0 - 640.0).abs() < 0.01);
        assert!((centre.1 - 360.0).abs() < 0.01);
        for id in [SlotId::FurnaceInput, SlotId::FurnaceFuel, SlotId::FurnaceOutput] {
            assert!(layout.slot_rect(id).is_some());
        }
        let input = layout.slot_rect(SlotId::FurnaceInput).unwrap();
        let fuel = layout.slot_rect(SlotId::FurnaceFuel).unwrap();
        let output = layout.slot_rect(SlotId::FurnaceOutput).unwrap();
        assert!(fuel.y >= input.y + input.h);
        assert!(output.x > input.x);
        for slot in &layout.slots {
            assert!(layout.panel.contains(slot.rect.x, slot.rect.y));
            assert!(
                layout
                    .panel
                    .contains(slot.rect.x + slot.rect.w - 0.1, slot.rect.y + slot.rect.h - 0.1)
            );
        }
        assert_eq!(
            layout.hit_test(input.center().0, input.center().1),
            Some(SlotId::FurnaceInput)
        );
        assert_eq!(
            layout.hit_test(output.center().0, output.center().1),
            Some(SlotId::FurnaceOutput)
        );
        assert_eq!(layout.hit_test(1.0, 1.0), None);
        assert_eq!(layout.hotbar_rects().len(), HOTBAR_SLOTS);
        let cook = layout.cook_bar.expect("cook bar");
        assert!(cook.x > input.x + input.w);
        assert!(cook.y + cook.h < layout.footer_top);
        assert!(layout.flame.is_some());
    }

    #[test]
    fn chest_layout_has_27_storage_slots_above_the_hotbar() {
        let screen = [1280.0, 720.0];
        let layout = chest_layout(screen);
        assert_eq!(layout.slots.len(), CHEST_SLOTS + HOTBAR_SLOTS);
        assert_eq!(layout.panel.w, CONTAINER_PANEL_WIDTH);
        assert_eq!(layout.panel.h, CHEST_PANEL_HEIGHT);
        let first = layout.slot_rect(SlotId::Chest(0)).unwrap();
        let ninth = layout.slot_rect(SlotId::Chest(8)).unwrap();
        let second_row = layout.slot_rect(SlotId::Chest(9)).unwrap();
        assert_eq!(first.y, ninth.y);
        assert!(second_row.y > first.y);
        let hotbar = layout.hotbar_rects();
        assert_eq!(hotbar.len(), HOTBAR_SLOTS);
        assert!(hotbar[0].y > layout.slot_rect(SlotId::Chest(26)).unwrap().y);
        assert_eq!(
            layout.hit_test(ninth.center().0, ninth.center().1),
            Some(SlotId::Chest(8))
        );
        assert_eq!(
            layout.hit_test(hotbar[4].center().0, hotbar[4].center().1),
            Some(SlotId::Hotbar(4))
        );
    }

    #[test]
    fn taking_from_the_furnace_uses_host_slot_actions() {
        let registry = registry();
        let session = furnace_session();
        let input = stack(&registry, "openblock:raw_iron", 3);
        let fuel = stack(&registry, "openblock:coal", 8);
        let output = stack(&registry, "openblock:iron_ingot", 2);
        let view = furnace_view(Some(input), Some(fuel), Some(output));

        let outcome = session.click(&registry, &view, SlotId::FurnaceInput, ClickButton::Left);
        assert_eq!(
            outcome.action,
            Some(ContainerAction::TakeFurnace {
                pos: session.pos,
                slot: FurnaceSlot::Input,
                count: 3
            })
        );
        assert_eq!(outcome.cursor_after, Some(input));

        let half = session.click(&registry, &view, SlotId::FurnaceInput, ClickButton::Right);
        assert_eq!(
            half.action,
            Some(ContainerAction::TakeFurnace {
                pos: session.pos,
                slot: FurnaceSlot::Input,
                count: 2
            })
        );
        assert_eq!(half.cursor_after.unwrap().count, 2);

        let output = session.click(&registry, &view, SlotId::FurnaceOutput, ClickButton::Right);
        assert_eq!(
            output.action,
            Some(ContainerAction::TakeFurnace {
                pos: session.pos,
                slot: FurnaceSlot::Output,
                count: 2
            })
        );
    }

    #[test]
    fn output_slot_is_take_only_and_merges_into_a_matching_cursor() {
        let registry = registry();
        let session = furnace_session();
        let ingot = stack(&registry, "openblock:iron_ingot", 2);
        let mut view = furnace_view(None, None, Some(ingot));

        let mut held = session.clone();
        held.cursor = Some(stack(&registry, "openblock:iron_ingot", 5));
        let merged = held.click(&registry, &view, SlotId::FurnaceOutput, ClickButton::Left);
        assert_eq!(
            merged.action,
            Some(ContainerAction::TakeFurnace {
                pos: session.pos,
                slot: FurnaceSlot::Output,
                count: 2
            })
        );
        assert_eq!(merged.cursor_after.unwrap().count, 7);

        held.cursor = Some(stack(&registry, "openblock:coal", 4));
        let refused = held.click(&registry, &view, SlotId::FurnaceOutput, ClickButton::Left);
        assert_eq!(refused.action, None);
        assert!(refused.message.unwrap().contains("take-only"));

        // Output is not a legal destination even for a matching item when it
        // would overflow the stack.
        held.cursor = Some(stack(&registry, "openblock:iron_ingot", 63));
        let full = held.click(&registry, &view, SlotId::FurnaceOutput, ClickButton::Left);
        assert_eq!(full.action, None);
        assert!(full.message.unwrap().contains("room"));

        // Input/fuel accept placement; a different-item left click swaps.
        view.furnace.as_mut().unwrap().fuel = Some(stack(&registry, "openblock:coal", 2));
        let mut coal = session.clone();
        coal.cursor = Some(stack(&registry, "openblock:planks", 8));
        let swap = coal.click(&registry, &view, SlotId::FurnaceFuel, ClickButton::Left);
        assert_eq!(
            swap.action,
            Some(ContainerAction::PutFurnace {
                pos: session.pos,
                slot: FurnaceSlot::Fuel,
                stack: stack(&registry, "openblock:planks", 8)
            })
        );
        assert_eq!(swap.cursor_after, Some(stack(&registry, "openblock:coal", 2)));
    }

    #[test]
    fn merging_respects_content_stack_limits_and_single_right_click_placement() {
        let registry = registry();
        let session = furnace_session();
        let coal = stack(&registry, "openblock:coal", 16);
        let mut view = furnace_view(None, Some(coal), None);
        let mut held = session.clone();
        held.cursor = Some(stack(&registry, "openblock:coal", 60));

        let outcome = held.click(&registry, &view, SlotId::FurnaceFuel, ClickButton::Left);
        assert_eq!(
            outcome.action,
            Some(ContainerAction::PutFurnace {
                pos: session.pos,
                slot: FurnaceSlot::Fuel,
                stack: stack(&registry, "openblock:coal", 64)
            })
        );
        assert_eq!(outcome.cursor_after.unwrap().count, 12);

        let outcome = held.click(&registry, &view, SlotId::FurnaceFuel, ClickButton::Right);
        assert_eq!(
            outcome.action,
            Some(ContainerAction::PutFurnace {
                pos: session.pos,
                slot: FurnaceSlot::Fuel,
                stack: stack(&registry, "openblock:coal", 17)
            })
        );
        assert_eq!(outcome.cursor_after.unwrap().count, 59);

        // An empty slot accepts one item on right click.
        view.furnace.as_mut().unwrap().fuel = None;
        let outcome = held.click(&registry, &view, SlotId::FurnaceFuel, ClickButton::Right);
        assert_eq!(
            outcome.action,
            Some(ContainerAction::PutFurnace {
                pos: session.pos,
                slot: FurnaceSlot::Fuel,
                stack: stack(&registry, "openblock:coal", 1)
            })
        );
        assert_eq!(outcome.cursor_after.unwrap().count, 59);
    }

    #[test]
    fn chest_clicks_transfer_between_storage_and_hotbar() {
        let registry = registry();
        let session = chest_session();
        let mut view = ContainerSnapshot::default();
        view.chest[0] = Some(stack(&registry, "openblock:cobblestone", 64));
        view.chest[1] = Some(stack(&registry, "openblock:planks", 3));
        view.inventory[2] = Some(stack(&registry, "openblock:apple", 5));

        let taken = session.click(&registry, &view, SlotId::Chest(0), ClickButton::Left);
        assert_eq!(
            taken.action,
            Some(ContainerAction::TakeChest {
                pos: session.pos,
                slot: 0,
                count: 64
            })
        );
        assert_eq!(taken.cursor_after.unwrap().count, 64);

        let mut held = session.clone();
        held.cursor = Some(stack(&registry, "openblock:cobblestone", 10));

        // Right-click placing into a different-item slot is refused instead of
        // replacing (and losing) the old stack.
        let refused_swap = held.click(&registry, &view, SlotId::Chest(1), ClickButton::Right);
        assert_eq!(refused_swap.action, None);
        assert!(refused_swap.message.unwrap().contains("Right-click"));

        let swap = held.click(&registry, &view, SlotId::Chest(1), ClickButton::Left);
        assert_eq!(
            swap.action,
            Some(ContainerAction::PutChest {
                pos: session.pos,
                slot: 1,
                stack: stack(&registry, "openblock:cobblestone", 10)
            })
        );
        assert_eq!(swap.cursor_after, Some(stack(&registry, "openblock:planks", 3)));

        // Right-click merges a matching item into the target slot.
        view.chest[2] = Some(stack(&registry, "openblock:cobblestone", 3));
        held.cursor = Some(stack(&registry, "openblock:cobblestone", 10));
        let merge = held.click(&registry, &view, SlotId::Chest(2), ClickButton::Right);
        assert_eq!(
            merge.action,
            Some(ContainerAction::PutChest {
                pos: session.pos,
                slot: 2,
                stack: stack(&registry, "openblock:cobblestone", 4)
            })
        );
        assert_eq!(merge.cursor_after.unwrap().count, 9);

        // Hotbar right click gives one item back; a click with an empty cursor
        // requests a survival hotbar take.
        let give = held.click(&registry, &view, SlotId::Hotbar(4), ClickButton::Right);
        assert_eq!(give.cursor_after.unwrap().count, 9);
        assert!(matches!(
            give.action,
            Some(ContainerAction::GiveInventory { stack }) if stack.count == 1
        ));
        let take = session.click(&registry, &view, SlotId::Hotbar(2), ClickButton::Left);
        assert_eq!(take.action, Some(ContainerAction::TakeHotbar { slot: 2, count: 5 }));
        assert_eq!(take.cursor_after.unwrap().count, 5);

        // Cross-kind slot addresses are refused.
        let refused = session.click(&registry, &view, SlotId::FurnaceInput, ClickButton::Left);
        assert_eq!(refused.action, None);
        assert!(refused.message.is_some());
    }

    #[test]
    fn arrow_keys_move_between_slots() {
        let screen = [1280.0, 720.0];
        let layout = furnace_layout(screen);
        let target = move_selection(&layout, SlotId::FurnaceInput, 1, 0);
        assert_eq!(target, SlotId::FurnaceOutput);
        let down = move_selection(&layout, SlotId::FurnaceInput, 0, 1);
        assert_eq!(down, SlotId::FurnaceFuel);
        assert_eq!(
            move_selection(&layout, SlotId::FurnaceInput, -1, 0),
            SlotId::FurnaceInput
        );

        let chest = chest_layout(screen);
        assert_eq!(move_selection(&chest, SlotId::Chest(0), 1, 0), SlotId::Chest(1));
        assert_eq!(move_selection(&chest, SlotId::Chest(0), 0, 1), SlotId::Chest(9));
    }

    #[test]
    fn layouts_survive_a_narrow_window() {
        let screen = [640.0, 360.0];
        for kind in [ContainerKind::Furnace, ContainerKind::Chest] {
            let layout = container_layout(kind, screen);
            assert!(layout.panel.x >= 0.0);
            assert!(layout.panel.y >= 0.0);
            assert!(layout.panel.x + layout.panel.w <= screen[0]);
            assert!(layout.panel.y + layout.panel.h <= screen[1]);
            for slot in &layout.slots {
                assert!(layout.panel.contains(slot.rect.x, slot.rect.y));
                assert!(
                    layout
                        .panel
                        .contains(slot.rect.x + slot.rect.w - 0.1, slot.rect.y + slot.rect.h - 0.1)
                );
            }
        }
    }

    #[test]
    fn unregistered_item_ids_are_treated_as_single_stacks() {
        let registry = registry();
        let fake = ItemStack::new(ItemId(u16::MAX), 5);
        assert_eq!(max_stack(&registry, fake.item), 1);
        assert!(!stacks_merge(fake, ItemStack::new(ItemId(0), 1)));
    }
}

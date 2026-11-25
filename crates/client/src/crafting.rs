//! Crafting screen model, list layout and drawing.
//!
//! The screen is a client presentation of the authoritative recipe and
//! inventory state: [`crafting_rows`] turns the registry plus
//! [`ob_server::Host::available_recipes`] into drawable rows, and
//! [`draw_crafting_screen`] renders them with the same modal panel language as
//! the settings screen. Selection, scrolling and hit-testing are GPU-free so
//! the screen is unit-testable without a window.

use ob_content::{ItemId, RecipeId, Registry};
use ob_render::UiFrame;
use ob_server::ItemStack;

use crate::inventory::{self, display_name};
use crate::ui_state::UiRect;

/// Width of the crafting panel in UI pixels.
pub const CRAFTING_PANEL_WIDTH: f32 = 620.0;
/// Height of the crafting panel in UI pixels.
///
/// The expanded 25-recipe catalogue needs more vertical room than the old
/// 11-recipe screen. At 720p the taller panel keeps the Basics heading and the
/// first iron-tool rows visible at scroll zero; the rest is still reached by
/// wheel/arrow/page scrolling.
pub const CRAFTING_PANEL_HEIGHT: f32 = 672.0;
/// Horizontal inner margin.
pub const CRAFTING_MARGIN: f32 = 28.0;
/// Height of one recipe row.
pub const CRAFTING_ROW_HEIGHT: f32 = 30.0;
/// Height of a section heading; the section gap is visible because the heading
/// is taller than the previous content.
pub const CRAFTING_SECTION_HEIGHT: f32 = 26.0;
/// Vertical distance from the panel top to the list content.
pub const CRAFTING_CONTENT_TOP: f32 = 88.0;
/// Space reserved below the list for the item info line and key hints.
pub const CRAFTING_FOOTER_HEIGHT: f32 = 80.0;
/// Pixels scrolled per wheel notch (one row pitch).
pub const CRAFTING_SCROLL_STEP: usize = CRAFTING_ROW_HEIGHT as usize;
/// Width of the list scrollbar track in UI pixels.
pub const CRAFTING_SCROLLBAR_WIDTH: f32 = 3.0;
/// Minimum height of the scrollbar thumb in UI pixels.
pub const CRAFTING_SCROLLBAR_MIN_THUMB: f32 = 18.0;

/// Footer key-hint line shown at the bottom of the crafting panel.
pub const CRAFTING_KEY_HINT: &str = "Up/Down select  PgUp/PgDn page  Wheel  Enter/Click craft  C/Esc back";

/// Which section a recipe belongs to.
///
/// Sections are emitted in the order of [`CraftSection::ALL`]; recipes inside a
/// section stay in deterministic registry order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CraftSection {
    /// Hand recipes (2x2): planks, sticks and the crafting table.
    Basics,
    /// Table recipes that produce placeable furniture: furnace and chest.
    Building,
    /// Table recipes that produce tools, in wood/stone/iron registry order.
    Tools,
    /// Table recipes that produce leather and iron armor.
    Armor,
}

impl CraftSection {
    /// Every section in drawing order.
    pub const ALL: [Self; 4] = [Self::Basics, Self::Building, Self::Tools, Self::Armor];

    /// Heading drawn above the section.
    pub fn title(self) -> &'static str {
        match self {
            Self::Basics => "Basics",
            Self::Building => "Building",
            Self::Tools => "Tools",
            Self::Armor => "Armor",
        }
    }
}

/// Current craftability of one recipe row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CraftStatus {
    /// Craftable now; the value is how many times the ingredients allow it.
    Craftable(u8),
    /// Missing ingredients.
    Missing,
    /// The recipe needs a crafting table but none is nearby.
    NeedsTable,
    /// Creative mode bypasses ingredients and the table.
    Creative,
}

impl CraftStatus {
    /// Short right-aligned label for the row.
    pub fn label(self) -> String {
        match self {
            Self::Craftable(count) => format!("x{count}"),
            Self::Missing => "missing".to_string(),
            Self::NeedsTable => "needs crafting table".to_string(),
            Self::Creative => "available".to_string(),
        }
    }

    /// Whether activating this row can succeed.
    pub fn can_craft(self) -> bool {
        matches!(self, Self::Craftable(_) | Self::Creative)
    }
}

/// One ingredient requirement with the player's current stock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngredientStatus {
    /// Ingredient item id.
    pub item: ItemId,
    /// Human-readable name (`Planks`).
    pub name: String,
    /// Amount required by one craft.
    pub need: u8,
    /// Amount currently held across the hotbar.
    pub have: u16,
    /// Whether the player has enough (always true in creative).
    pub enough: bool,
}

impl IngredientStatus {
    /// `3 Planks` style summary fragment.
    pub fn summary(&self) -> String {
        format!("{} {}", self.need, pluralized(&self.name, self.need))
    }
}

/// One drawable crafting recipe row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CraftingRow {
    /// Stable dense recipe id used by [`ob_server::Host::craft`].
    pub recipe: RecipeId,
    /// "Basics" or "Tools".
    pub section: CraftSection,
    /// Output item and count.
    pub output: (ItemId, u8),
    /// Title-cased output name.
    pub output_name: String,
    /// Ingredient requirements and current stock.
    pub ingredients: Vec<IngredientStatus>,
    /// Combined summary such as `3 Planks + 2 Sticks`.
    pub ingredient_summary: String,
    /// Current availability.
    pub status: CraftStatus,
    /// Whether the recipe needs a nearby crafting table.
    pub needs_table: bool,
}

impl CraftingRow {
    /// Whether the recipe can be crafted right now.
    pub fn can_craft(&self) -> bool {
        self.status.can_craft()
    }

    /// `Planks x4` style output label.
    pub fn output_label(&self) -> String {
        if self.output.1 > 1 {
            format!("{} x{}", self.output_name, self.output.1)
        } else {
            self.output_name.clone()
        }
    }

    /// Durability/speed, armor, food or block hint for the footer info line.
    pub fn output_info(&self, registry: &Registry) -> String {
        inventory::item_info_hint(registry, self.output.0, None)
    }
}

/// Applies English-style pluralisation used by ingredient summaries.
fn pluralized(name: &str, count: u8) -> String {
    // Material nouns in the catalogue are uncountable ("8 Leather", not
    // "8 Leathers"); the rest follow the simple plural rule below.
    let uncountable = matches!(name, "Leather" | "Coal" | "Charcoal" | "Beef" | "Porkchop");
    if count <= 1 {
        if name.ends_with('s') && !name.ends_with("ss") && !uncountable {
            name[..name.len() - 1].to_string()
        } else {
            name.to_string()
        }
    } else if name.ends_with('s') || uncountable {
        name.to_string()
    } else {
        format!("{name}s")
    }
}

/// Classifies a recipe into the crafting-screen section it belongs to.
///
/// Hand recipes are always Basics; table recipes are grouped by what they
/// produce. Unknown table recipes land in Building so a custom content pack
/// still shows every row exactly once.
pub fn section_for_recipe(registry: &Registry, recipe: &ob_content::Recipe) -> CraftSection {
    if !recipe.needs_table {
        return CraftSection::Basics;
    }
    if recipe.output.0.0 as usize >= registry.item_len() {
        return CraftSection::Building;
    }
    let item = registry.item(recipe.output.0);
    if item.tool.is_some() {
        CraftSection::Tools
    } else if item.armor.is_some() {
        CraftSection::Armor
    } else {
        CraftSection::Building
    }
}

/// Builds the complete row list for the crafting screen.
///
/// Rows are grouped into [`CraftSection::ALL`] order (Basics, Building, Tools,
/// Armor) and keep deterministic recipe-id order inside each section. `available`
/// is the host's [`ob_server::Host::available_recipes`] snapshot; `creative`
/// bypasses ingredients and table gating just like [`ob_server::Host::craft`].
pub fn crafting_rows(
    registry: &Registry,
    available: &[(RecipeId, u8)],
    near_table: bool,
    creative: bool,
    inventory: &[Option<ItemStack>],
) -> Vec<CraftingRow> {
    let count_for = |recipe: RecipeId| -> u8 {
        available
            .iter()
            .find(|(id, _)| *id == recipe)
            .map(|(_, count)| *count)
            .unwrap_or(0)
    };
    let mut rows = Vec::with_capacity(registry.recipe_len());
    for section in CraftSection::ALL {
        for (recipe_id, recipe) in registry.recipes() {
            if section_for_recipe(registry, recipe) != section {
                continue;
            }
            let needs_table = recipe.needs_table;
            let ingredients: Vec<IngredientStatus> = recipe
                .ingredients
                .iter()
                .map(|(item, need)| {
                    let name = display_name(registry.item(*item).id);
                    let have: u16 = inventory
                        .iter()
                        .flatten()
                        .filter(|stack| stack.item == *item)
                        .map(|stack| u16::from(stack.count))
                        .sum();
                    IngredientStatus {
                        item: *item,
                        name,
                        need: *need,
                        have,
                        enough: creative || have >= u16::from(*need),
                    }
                })
                .collect();
            let ingredient_summary = ingredients
                .iter()
                .map(IngredientStatus::summary)
                .collect::<Vec<_>>()
                .join(" + ");
            let status = if creative {
                CraftStatus::Creative
            } else if needs_table && !near_table {
                CraftStatus::NeedsTable
            } else {
                match count_for(recipe_id) {
                    0 => CraftStatus::Missing,
                    count => CraftStatus::Craftable(count),
                }
            };
            rows.push(CraftingRow {
                recipe: recipe_id,
                section,
                output: recipe.output,
                output_name: recipe.display_name.to_string(),
                ingredients,
                ingredient_summary,
                status,
                needs_table,
            });
        }
    }
    rows
}

/// Selection, hover and scroll state of the crafting screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CraftingState {
    /// Selected recipe-row index into [`crafting_rows`].
    pub selected: usize,
    /// First visible list pixel offset.
    pub scroll: usize,
    /// Hovered row index, if any.
    pub hovered: Option<usize>,
}

impl CraftingState {
    /// Resets focus for a freshly opened screen.
    pub fn open(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.hovered = None;
    }

    /// Number of rows (zero is valid for a custom registry without recipes).
    fn clamp_selection(&mut self, row_count: usize) {
        if row_count == 0 {
            self.selected = 0;
            self.hovered = None;
            self.scroll = 0;
        } else {
            self.selected = self.selected.min(row_count - 1);
        }
    }

    /// Moves selection by `steps` rows, wrapping around the list.
    pub fn move_selection(&mut self, steps: i32, row_count: usize) {
        if row_count == 0 || steps == 0 {
            self.clamp_selection(row_count);
            return;
        }
        self.selected = (self.selected as i32 + steps).rem_euclid(row_count as i32) as usize;
        self.hovered = None;
    }

    /// Selected row index, if the list has one.
    pub fn selected_index(&self, row_count: usize) -> Option<usize> {
        (row_count > 0 && self.selected < row_count).then_some(self.selected)
    }

    /// Recipe to send to [`ob_server::Host::craft`] for the current selection.
    pub fn selected_recipe(&self, rows: &[CraftingRow]) -> Option<RecipeId> {
        rows.get(self.selected).map(|row| row.recipe)
    }

    /// Scrolls by `steps` wheel notches, clamped to the list.
    pub fn scroll_by(&mut self, steps: i32, screen: [f32; 2], rows: &[CraftingRow]) {
        let layout = crafting_layout(screen, rows, self.scroll);
        let delta = i64::from(steps) * CRAFTING_SCROLL_STEP as i64;
        self.scroll =
            (i64::try_from(layout.scroll).unwrap_or(i64::MAX) + delta).clamp(0, layout.max_scroll as i64) as usize;
    }

    /// Number of whole rows that fit in the list viewport (at least one).
    pub fn page_rows(screen: [f32; 2], rows: &[CraftingRow]) -> usize {
        let layout = crafting_layout(screen, rows, 0);
        ((layout.content.h / CRAFTING_ROW_HEIGHT).floor() as usize).max(1)
    }

    /// Moves the selection by one viewport page and keeps it visible.
    pub fn page_by(&mut self, direction: i32, screen: [f32; 2], rows: &[CraftingRow]) {
        if rows.is_empty() || direction == 0 {
            return;
        }
        let page = Self::page_rows(screen, rows) as i32;
        self.move_selection(direction.signum() * page, rows.len());
        self.ensure_visible(screen, rows);
    }

    /// Updates hover (and keyboard selection) from the cursor position.
    pub fn update_hover(&mut self, cursor: (f32, f32), screen: [f32; 2], rows: &[CraftingRow]) -> Option<usize> {
        let layout = crafting_layout(screen, rows, self.scroll);
        self.scroll = layout.scroll;
        self.hovered = layout
            .rows
            .iter()
            .find(|row| {
                layout.content.contains(cursor.0, cursor.1)
                    && row_fully_visible(row.rect, layout.content)
                    && row.rect.contains(cursor.0, cursor.1)
            })
            .map(|row| row.index);
        if let Some(index) = self.hovered {
            self.selected = index;
        }
        self.hovered
    }

    /// Handles a click on a row, returning the recipe to craft if one was hit.
    pub fn click(&mut self, cursor: (f32, f32), screen: [f32; 2], rows: &[CraftingRow]) -> Option<RecipeId> {
        let index = self.update_hover(cursor, screen, rows)?;
        rows.get(index).map(|row| row.recipe)
    }

    /// Adjusts the scroll offset so the selected row is visible.
    pub fn ensure_visible(&mut self, screen: [f32; 2], rows: &[CraftingRow]) {
        if rows.is_empty() {
            self.scroll = 0;
            return;
        }
        self.clamp_selection(rows.len());
        let mut layout = crafting_layout(screen, rows, self.scroll);
        let Some(row) = layout.rows.get(self.selected) else {
            return;
        };
        let content_bottom = layout.content.y + layout.content.h;
        if row.rect.y < layout.content.y {
            let delta = (layout.content.y - row.rect.y).ceil() as usize;
            self.scroll = self.scroll.saturating_sub(delta);
        } else if row.rect.y + row.rect.h > content_bottom {
            let delta = (row.rect.y + row.rect.h - content_bottom).ceil() as usize;
            self.scroll = self.scroll.saturating_add(delta);
        }
        layout = crafting_layout(screen, rows, self.scroll);
        self.scroll = layout.scroll;
    }
}

/// One row rectangle in screen space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CraftingRowRect {
    /// Index into [`crafting_rows`].
    pub index: usize,
    /// Full clickable rectangle.
    pub rect: UiRect,
}

/// Computed geometry of the crafting screen.
#[derive(Debug, Clone, PartialEq)]
pub struct CraftingLayout {
    /// Modal panel rectangle.
    pub panel: UiRect,
    /// Clipped list viewport.
    pub content: UiRect,
    /// Every row's rectangle (including rows currently clipped away).
    pub rows: Vec<CraftingRowRect>,
    /// Effective clamped scroll offset.
    pub scroll: usize,
    /// Largest useful scroll offset.
    pub max_scroll: usize,
}

/// Bounds of the centered crafting panel for a target size.
pub fn crafting_panel_rect(screen: [f32; 2]) -> UiRect {
    let width = CRAFTING_PANEL_WIDTH.min((screen[0] - 24.0).max(1.0));
    let height = CRAFTING_PANEL_HEIGHT.min((screen[1] - 24.0).max(1.0));
    UiRect::new((screen[0] - width) / 2.0, (screen[1] - height) / 2.0, width, height)
}

/// Total pixel height of the list including section headings.
fn list_height(rows: &[CraftingRow]) -> f32 {
    let mut previous = None;
    let mut headings = 0usize;
    for row in rows {
        if previous != Some(row.section) {
            headings += 1;
            previous = Some(row.section);
        }
    }
    rows.len() as f32 * CRAFTING_ROW_HEIGHT + headings as f32 * CRAFTING_SECTION_HEIGHT
}

/// Computes panel, list and row rectangles for a scroll offset.
///
/// The list is laid out even when rows fall outside [`CraftingLayout::content`];
/// drawing and hit-testing filter against the viewport so partially visible
/// rows still work.
pub fn crafting_layout(screen: [f32; 2], rows: &[CraftingRow], scroll: usize) -> CraftingLayout {
    let panel = crafting_panel_rect(screen);
    let content = UiRect::new(
        panel.x + CRAFTING_MARGIN,
        panel.y + CRAFTING_CONTENT_TOP,
        (panel.w - CRAFTING_MARGIN * 2.0).max(0.0),
        (panel.h - CRAFTING_CONTENT_TOP - CRAFTING_FOOTER_HEIGHT).max(0.0),
    );
    let total = list_height(rows);
    let max_scroll = (total - content.h).max(0.0) as usize;
    let scroll = scroll.min(max_scroll);
    let mut row_rects = Vec::with_capacity(rows.len());
    let mut y = content.y - scroll as f32;
    let mut previous = None;
    for (index, row) in rows.iter().enumerate() {
        if previous != Some(row.section) {
            y += CRAFTING_SECTION_HEIGHT;
            previous = Some(row.section);
        }
        row_rects.push(CraftingRowRect {
            index,
            rect: UiRect::new(content.x, y, content.w, CRAFTING_ROW_HEIGHT),
        });
        y += CRAFTING_ROW_HEIGHT;
    }
    CraftingLayout {
        panel,
        content,
        rows: row_rects,
        scroll,
        max_scroll,
    }
}

fn draw_panel(frame: &mut UiFrame, panel: UiRect) {
    frame.rect(
        0.0,
        0.0,
        frame.screen_size().0,
        frame.screen_size().1,
        [0.0, 0.0, 0.0, 0.55],
    );
    frame.rect(
        0.0,
        0.0,
        frame.screen_size().0,
        frame.screen_size().1,
        [0.02, 0.03, 0.06, 0.18],
    );
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

fn availability_color(status: CraftStatus) -> [f32; 4] {
    match status {
        CraftStatus::Craftable(_) => [0.68, 0.92, 0.52, 1.0],
        CraftStatus::Missing => [0.72, 0.76, 0.83, 1.0],
        CraftStatus::NeedsTable => [0.96, 0.72, 0.38, 1.0],
        CraftStatus::Creative => [0.98, 0.83, 0.44, 1.0],
    }
}

/// Whether a row rectangle is fully inside the list viewport.
///
/// Partially clipped rows are not drawn or hit-tested: the row painter has no
/// scissor, so drawing a half-visible row would spill text into the footer.
/// Scrolling to a row via [`CraftingState::ensure_visible`] makes it fully
/// visible, and the clamped scroll positions keep the first/last row whole.
fn row_fully_visible(row: UiRect, content: UiRect) -> bool {
    row.y >= content.y - 0.01 && row.y + row.h <= content.y + content.h + 0.01
}

/// Draws the thin scrollbar beside the list when more rows are hidden.
fn draw_scrollbar(frame: &mut UiFrame, layout: &CraftingLayout) {
    if layout.max_scroll == 0 || layout.content.h <= 0.0 {
        return;
    }
    let x = layout.panel.x + layout.panel.w - 12.0;
    let track_h = layout.content.h;
    let total = track_h + layout.max_scroll as f32;
    let thumb_h = (track_h * track_h / total)
        .max(CRAFTING_SCROLLBAR_MIN_THUMB)
        .min(track_h);
    let progress = layout.scroll as f32 / layout.max_scroll as f32;
    let thumb_y = layout.content.y + (track_h - thumb_h) * progress;
    frame.rect(
        x,
        layout.content.y,
        CRAFTING_SCROLLBAR_WIDTH,
        track_h,
        [0.14, 0.17, 0.22, 0.95],
    );
    frame.rect(x, thumb_y, CRAFTING_SCROLLBAR_WIDTH, thumb_h, [0.58, 0.64, 0.74, 0.95]);
}

/// Draws the crafting screen over the dimmed world.
pub fn draw_crafting_screen(
    frame: &mut UiFrame,
    state: &CraftingState,
    rows: &[CraftingRow],
    registry: &Registry,
    near_table: bool,
    creative: bool,
    screen: [f32; 2],
) {
    let layout = crafting_layout(screen, rows, state.scroll);
    let panel = layout.panel;
    draw_panel(frame, panel);

    frame.text(
        panel.x + CRAFTING_MARGIN,
        panel.y + 16.0,
        2.6,
        [0.98, 0.99, 1.0, 1.0],
        "Crafting",
    );
    let subtitle = if near_table {
        "3x3 crafting table nearby"
    } else {
        "2x2 hand crafting"
    };
    frame.text(
        panel.x + CRAFTING_MARGIN,
        panel.y + 48.0,
        1.2,
        [0.66, 0.73, 0.82, 1.0],
        subtitle,
    );
    if creative {
        let label = format!("Creative: all {} recipes available", rows.len());
        let width = frame.text_width(1.1, &label);
        frame.text(
            panel.x + panel.w - CRAFTING_MARGIN - width,
            panel.y + 49.0,
            1.1,
            [0.98, 0.83, 0.44, 1.0],
            &label,
        );
    } else {
        let label = format!("{} recipes", rows.len());
        let width = frame.text_width(1.1, &label);
        frame.text(
            panel.x + panel.w - CRAFTING_MARGIN - width,
            panel.y + 49.0,
            1.1,
            [0.66, 0.73, 0.82, 1.0],
            &label,
        );
    }
    frame.rect(
        panel.x + CRAFTING_MARGIN,
        panel.y + 68.0,
        panel.w - CRAFTING_MARGIN * 2.0,
        1.0,
        [0.24, 0.28, 0.35, 1.0],
    );

    let mut previous_section = None;
    let content_bottom = layout.content.y + layout.content.h;
    for row_rect in &layout.rows {
        let Some(row) = rows.get(row_rect.index) else {
            continue;
        };
        if previous_section != Some(row.section) {
            let header_y = row_rect.rect.y - CRAFTING_SECTION_HEIGHT;
            if header_y >= layout.content.y - 0.01 && header_y + CRAFTING_SECTION_HEIGHT <= content_bottom + 0.01 {
                frame.text(
                    layout.content.x + 2.0,
                    header_y + 7.0,
                    1.15,
                    [0.98, 0.80, 0.42, 1.0],
                    row.section.title(),
                );
                let line_x = layout.content.x + 82.0;
                if line_x < layout.content.x + layout.content.w {
                    frame.rect(
                        line_x,
                        header_y + 14.0,
                        layout.content.x + layout.content.w - line_x,
                        1.0,
                        [0.22, 0.26, 0.33, 0.9],
                    );
                }
            }
            previous_section = Some(row.section);
        }
        let rect = row_rect.rect;
        if !row_fully_visible(rect, layout.content) {
            continue;
        }
        let selected = row_rect.index == state.selected;
        let hovered = state.hovered == Some(row_rect.index);
        let background = if selected {
            [0.16, 0.21, 0.29, 0.96]
        } else if hovered {
            [0.12, 0.16, 0.22, 0.94]
        } else if row.can_craft() {
            [0.08, 0.10, 0.14, 0.90]
        } else {
            [0.07, 0.08, 0.11, 0.84]
        };
        frame.rect(rect.x, rect.y, rect.w, rect.h, background);
        if selected {
            frame.outline(rect.x, rect.y, rect.w, rect.h, 1.0, [0.95, 0.79, 0.35, 0.95]);
            frame.rect(rect.x, rect.y + 4.0, 3.0, rect.h - 8.0, [0.96, 0.76, 0.32, 1.0]);
        } else if hovered {
            frame.outline(rect.x, rect.y, rect.w, rect.h, 1.0, [0.55, 0.62, 0.72, 0.8]);
        }

        let icon_size = 24.0;
        inventory::draw_item_icon_for(frame, registry, row.output.0, rect.x + 8.0, rect.y + 3.0, icon_size);

        let name = row.output_label();
        let name_color = if row.can_craft() {
            [0.94, 0.97, 1.0, 1.0]
        } else {
            [0.72, 0.77, 0.84, 1.0]
        };
        frame.text(rect.x + 40.0, rect.y + 4.0, 1.25, name_color, &name);

        let mut x = rect.x + 40.0;
        for (index, ingredient) in row.ingredients.iter().enumerate() {
            if index > 0 {
                let separator = " + ";
                frame.text(x, rect.y + 17.0, 1.0, [0.58, 0.63, 0.70, 1.0], separator);
                x += frame.text_width(1.0, separator);
            }
            let part = ingredient.summary();
            let color = if ingredient.enough {
                [0.60, 0.86, 0.48, 1.0]
            } else {
                [0.98, 0.55, 0.48, 1.0]
            };
            frame.text(x, rect.y + 17.0, 1.0, color, &part);
            x += frame.text_width(1.0, &part);
        }

        let status = row.status.label();
        let status_width = frame.text_width(1.0, &status);
        frame.text(
            rect.x + rect.w - 12.0 - status_width,
            rect.y + 10.0,
            1.0,
            availability_color(row.status),
            &status,
        );
    }
    if rows.is_empty() {
        frame.text_centered(
            panel.x + panel.w / 2.0,
            layout.content.y + 24.0,
            1.3,
            [0.72, 0.77, 0.84, 1.0],
            "No recipes in this registry",
        );
    } else {
        draw_scrollbar(frame, &layout);
    }

    let footer_top = panel.y + panel.h - CRAFTING_FOOTER_HEIGHT;
    frame.rect(
        panel.x + CRAFTING_MARGIN,
        footer_top,
        panel.w - CRAFTING_MARGIN * 2.0,
        1.0,
        [0.24, 0.28, 0.35, 1.0],
    );
    if let Some(row) = rows.get(state.selected) {
        let info = row.output_info(registry);
        let text = if info.is_empty() {
            row.output_label()
        } else {
            format!("{}   {}", row.output_label(), info)
        };
        frame.text(
            panel.x + CRAFTING_MARGIN,
            footer_top + 12.0,
            1.0,
            [0.86, 0.91, 1.0, 1.0],
            &text,
        );
        if row.needs_table && !near_table && !creative {
            let hint = "Place/stand near a crafting table to craft this";
            frame.text(
                panel.x + CRAFTING_MARGIN,
                footer_top + 28.0,
                1.0,
                [0.96, 0.72, 0.38, 1.0],
                hint,
            );
        }
    }
    frame.text_centered(
        panel.x + panel.w / 2.0,
        panel.y + panel.h - 22.0,
        1.0,
        [0.62, 0.68, 0.76, 1.0],
        CRAFTING_KEY_HINT,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_content::Registry;
    use ob_server::ItemStack;

    fn available(registry: &Registry, counts: &[(&str, u8)], near_table: bool) -> Vec<(RecipeId, u8)> {
        let mut out = Vec::new();
        for (name, count) in counts {
            let id = registry.recipe_id(name).expect("recipe id");
            let recipe = registry.recipe(id).expect("recipe");
            if recipe.needs_table && !near_table {
                out.push((id, 0));
            } else {
                out.push((id, *count));
            }
        }
        out
    }

    fn inventory(registry: &Registry, items: &[(&str, u8)]) -> Vec<Option<ItemStack>> {
        let mut slots = vec![None; 9];
        for (index, (name, count)) in items.iter().enumerate().take(9) {
            let item = registry.item_id(name).expect("item");
            slots[index] = Some(ItemStack::for_item(item, registry.item(item), *count));
        }
        slots
    }

    fn all_available(registry: &Registry) -> Vec<(RecipeId, u8)> {
        registry.recipes().map(|(id, _)| (id, 4)).collect()
    }

    #[test]
    fn rows_group_basics_building_tools_and_armor_in_recipe_order() {
        let registry = Registry::base();
        let rows = crafting_rows(
            &registry,
            &all_available(&registry),
            true,
            false,
            &inventory(&registry, &[]),
        );
        assert_eq!(rows.len(), 25);
        let sections: Vec<CraftSection> = rows.iter().map(|row| row.section).collect();
        assert_eq!(sections[..3], [CraftSection::Basics; 3]);
        assert_eq!(sections[3..5], [CraftSection::Building; 2]);
        assert_eq!(sections[5..17], [CraftSection::Tools; 12]);
        assert_eq!(sections[17..], [CraftSection::Armor; 8]);
        let names: Vec<&str> = rows.iter().map(|row| row.output_name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Planks",
                "Sticks",
                "Crafting Table",
                "Furnace",
                "Chest",
                "Wooden Pickaxe",
                "Wooden Axe",
                "Wooden Shovel",
                "Wooden Sword",
                "Stone Pickaxe",
                "Stone Axe",
                "Stone Shovel",
                "Stone Sword",
                "Iron Pickaxe",
                "Iron Axe",
                "Iron Shovel",
                "Iron Sword",
                "Leather Helmet",
                "Leather Chestplate",
                "Leather Leggings",
                "Leather Boots",
                "Iron Helmet",
                "Iron Chestplate",
                "Iron Leggings",
                "Iron Boots",
            ]
        );
        // Every section stays in deterministic registry recipe order.
        for pair in rows.windows(2).filter(|pair| pair[0].section == pair[1].section) {
            assert!(
                pair[0].recipe < pair[1].recipe,
                "{} must precede {}",
                pair[0].output_name,
                pair[1].output_name
            );
        }
        assert_eq!(rows[0].output_label(), "Planks x4");
        assert_eq!(rows[5].output_label(), "Wooden Pickaxe");
        assert_eq!(rows[13].output_label(), "Iron Pickaxe");
    }

    #[test]
    fn table_recipes_are_gated_and_creative_bypasses_the_table() {
        let registry = Registry::base();
        let available = available(
            &registry,
            &[
                ("openblock:planks", 2),
                ("openblock:sticks", 1),
                ("openblock:crafting_table", 1),
                ("openblock:wooden_pickaxe", 0),
            ],
            false,
        );
        let rows = crafting_rows(
            &registry,
            &available,
            false,
            false,
            &inventory(&registry, &[("openblock:planks", 4), ("openblock:stick", 2)]),
        );
        assert_eq!(rows[0].status, CraftStatus::Craftable(2));
        assert!(rows[0].can_craft());
        for row in &rows[3..] {
            assert_eq!(row.status, CraftStatus::NeedsTable, "{}", row.output_name);
            assert!(!row.can_craft());
        }

        let rows = crafting_rows(&registry, &available, false, true, &[]);
        assert!(rows.iter().all(|row| row.status == CraftStatus::Creative));
        assert!(rows.iter().all(CraftingRow::can_craft));
        assert_eq!(rows[3].status.label(), "available");
    }

    #[test]
    fn ingredient_summaries_and_counts_are_derived_from_the_inventory() {
        let registry = Registry::base();
        let rows = crafting_rows(
            &registry,
            &all_available(&registry),
            true,
            false,
            &inventory(&registry, &[("openblock:planks", 2), ("openblock:stick", 1)]),
        );
        assert_eq!(rows[0].ingredient_summary, "1 Log");
        assert_eq!(rows[1].ingredient_summary, "2 Planks");
        assert_eq!(rows[3].ingredient_summary, "8 Cobblestones");
        assert_eq!(rows[4].ingredient_summary, "8 Planks");
        assert_eq!(rows[5].ingredient_summary, "3 Planks + 2 Sticks");
        assert_eq!(rows[7].ingredient_summary, "1 Plank + 2 Sticks");
        assert_eq!(rows[9].ingredient_summary, "3 Cobblestones + 2 Sticks");
        assert_eq!(rows[13].ingredient_summary, "3 Iron Ingots + 2 Sticks");
        assert_eq!(rows[18].ingredient_summary, "8 Leather");
        assert_eq!(rows[22].ingredient_summary, "8 Iron Ingots");
        let pick = &rows[5];
        assert_eq!(pick.ingredients[0].have, 2);
        assert!(!pick.ingredients[0].enough);
        assert_eq!(pick.ingredients[1].have, 1);
        assert!(!pick.ingredients[1].enough);
        // A row can be missing ingredients without needing a table.
        assert_eq!(rows[1].status, CraftStatus::Craftable(4));
        // Larger ingredient totals are one status entry with per-item colours.
        let furnace = &rows[3];
        assert_eq!(furnace.ingredients.len(), 1);
        assert_eq!(furnace.ingredients[0].need, 8);
        assert!(!furnace.ingredients[0].enough);
    }

    #[test]
    fn status_prefers_table_gating_then_missing_ingredients() {
        let registry = Registry::base();
        let all_zero: Vec<(RecipeId, u8)> = registry.recipes().map(|(id, _)| (id, 0)).collect();
        let rows = crafting_rows(&registry, &all_zero, false, false, &[]);
        assert_eq!(rows[0].status, CraftStatus::Missing);
        assert_eq!(rows[3].status, CraftStatus::NeedsTable);
        assert_eq!(rows[3].status.label(), "needs crafting table");
        assert_eq!(
            rows.iter().filter(|row| row.status == CraftStatus::NeedsTable).count(),
            22,
            "every table recipe is gated while no crafting table is nearby"
        );
        assert!(
            rows.iter()
                .filter(|row| row.needs_table)
                .all(|row| row.section != CraftSection::Basics)
        );
        let rows = crafting_rows(&registry, &all_zero, true, false, &[]);
        assert_eq!(rows[0].status, CraftStatus::Missing);
        assert_eq!(rows[3].status, CraftStatus::Missing);
    }

    #[test]
    fn selection_wraps_both_ways_and_clamps_to_the_list() {
        let mut state = CraftingState::default();
        state.open();
        state.move_selection(-1, 25);
        assert_eq!(state.selected, 24);
        state.move_selection(1, 25);
        assert_eq!(state.selected, 0);
        state.move_selection(3, 25);
        assert_eq!(state.selected, 3);
        state.move_selection(100, 25);
        assert_eq!(state.selected, 3);
        state.move_selection(1, 0);
        assert_eq!(state.selected, 0);
        assert_eq!(state.selected_index(0), None);
    }

    #[test]
    fn recipe_rows_map_to_host_craft_requests() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        let mut state = CraftingState {
            selected: 13,
            ..CraftingState::default()
        };
        let recipe = state.selected_recipe(&rows).expect("selected recipe");
        assert_eq!(registry.recipe(recipe).expect("recipe").id, "openblock:iron_pickaxe");
        state.selected = 99;
        state.clamp_selection(rows.len());
        assert_eq!(state.selected, 24);
        assert_eq!(
            registry.recipe(state.selected_recipe(&rows).unwrap()).unwrap().id,
            "openblock:iron_boots"
        );
        state.selected = 23;
        assert_eq!(
            registry.recipe(state.selected_recipe(&rows).unwrap()).unwrap().id,
            "openblock:iron_leggings"
        );
    }

    #[test]
    fn keyboard_navigation_keeps_the_selected_row_visible() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        // A deliberately short window forces a scrollable list.
        let screen = [800.0, 300.0];
        let layout = crafting_layout(screen, &rows, 0);
        assert!(layout.max_scroll > 0);
        assert!(layout.rows[0].rect.y >= layout.content.y);
        let mut state = CraftingState::default();
        for _ in 0..rows.len() {
            state.move_selection(1, rows.len());
            state.ensure_visible(screen, &rows);
            let layout = crafting_layout(screen, &rows, state.scroll);
            let rect = layout.rows[state.selected].rect;
            assert!(
                rect.y >= layout.content.y - 0.01 && rect.y + rect.h <= layout.content.y + layout.content.h + 0.01,
                "selected row {} must be visible at scroll {}",
                state.selected,
                state.scroll
            );
        }
        state.selected = 0;
        state.scroll = usize::MAX;
        state.ensure_visible(screen, &rows);
        assert_eq!(state.scroll, crafting_layout(screen, &rows, usize::MAX).max_scroll);
    }

    #[test]
    fn mouse_hover_and_click_hit_recipe_rows() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        let screen = [1280.0, 720.0];
        let layout = crafting_layout(screen, &rows, 0);
        let target = layout.rows[4].rect.center();
        let mut state = CraftingState::default();
        assert_eq!(state.update_hover(target, screen, &rows), Some(4));
        assert_eq!(state.selected, 4);
        assert_eq!(state.click(target, screen, &rows), Some(rows[4].recipe));
        assert_eq!(state.update_hover((1.0, 1.0), screen, &rows), None);
        assert_eq!(state.click((1.0, 1.0), screen, &rows), None);
    }

    #[test]
    fn scroll_is_clamped_and_wheel_moves_by_row_pitch() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        let screen = [800.0, 300.0];
        let mut state = CraftingState::default();
        state.scroll_by(1, screen, &rows);
        assert_eq!(state.scroll, CRAFTING_SCROLL_STEP);
        state.scroll_by(999, screen, &rows);
        let layout = crafting_layout(screen, &rows, state.scroll);
        assert_eq!(state.scroll, layout.max_scroll);
        state.scroll_by(-999, screen, &rows);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn wheel_and_page_navigation_reach_the_last_recipe() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        assert_eq!(rows.len(), 25);
        let screen = [1280.0, 720.0];
        let page = CraftingState::page_rows(screen, &rows);
        assert!(page >= 2);
        let mut state = CraftingState::default();
        state.page_by(1, screen, &rows);
        assert_eq!(state.selected, page);
        assert!(state.scroll > 0, "a page move scrolls the selection into view");
        state.scroll_by(999, screen, &rows);
        let layout = crafting_layout(screen, &rows, state.scroll);
        let last = layout.rows.last().expect("last row").rect.center();
        assert_eq!(state.update_hover(last, screen, &rows), Some(24));
        assert_eq!(state.click(last, screen, &rows), Some(rows[24].recipe));
        assert_eq!(
            registry.recipe(rows[24].recipe).expect("recipe").id,
            "openblock:iron_boots"
        );
    }

    #[test]
    fn every_recipe_section_has_an_icon_and_an_info_line() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        let frame = UiFrame::new(1280.0, 720.0);
        let layout = crafting_layout([1280.0, 720.0], &rows, 0);
        for row in &rows {
            assert!(
                inventory::item_icon_for(&registry, row.output.0).is_some(),
                "{} needs a hotbar/crafting icon",
                row.output_name
            );
            let info = row.output_info(&registry);
            assert!(!info.is_empty(), "{} needs a footer info line", row.output_name);
            let footer = format!("{}   {}", row.output_label(), info);
            assert!(
                frame.text_width(1.0, &footer) <= layout.content.w,
                "{} footer text is {:.0}px wide, content is {:.0}px",
                row.output_name,
                frame.text_width(1.0, &footer),
                layout.content.w
            );
        }
        assert!(
            frame.text_width(1.0, CRAFTING_KEY_HINT) <= layout.content.w,
            "the key hint must fit inside the panel"
        );
        let iron_pick = rows
            .iter()
            .find(|row| row.output_name == "Iron Pickaxe")
            .expect("iron pick");
        assert!(iron_pick.output_info(&registry).contains("Durability 251/251"));
        assert!(iron_pick.output_info(&registry).contains("Speed 6.0"));
        let iron_chest = rows
            .iter()
            .find(|row| row.output_name == "Iron Chestplate")
            .expect("iron chestplate");
        assert_eq!(
            iron_chest.output_info(&registry),
            "Chestplate · Defence 6 · Durability 240/240"
        );
        let furnace = rows.iter().find(|row| row.output_name == "Furnace").expect("furnace");
        assert_eq!(furnace.output_info(&registry), "hardness 3.5 s");
    }

    #[test]
    fn layout_shows_the_grouped_twenty_five_recipes_and_scrolls() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        let screen = [1280.0, 720.0];
        let layout = crafting_layout(screen, &rows, 0);
        assert_eq!(layout.panel.w, CRAFTING_PANEL_WIDTH);
        assert_eq!(layout.panel.h, CRAFTING_PANEL_HEIGHT);
        assert!(layout.max_scroll > 0, "the expanded list must scroll at 720p");
        assert_eq!(layout.rows.len(), 25);
        assert!(list_height(&rows) > layout.content.h);
        assert_eq!(layout.rows[0].rect.y, layout.content.y + CRAFTING_SECTION_HEIGHT);
        assert!(layout.rows.iter().all(|row| row.rect.x >= layout.content.x));
        assert!(
            layout
                .rows
                .iter()
                .all(|row| row.rect.x + row.rect.w <= layout.content.x + layout.content.w)
        );
        // The scrollbar is only meaningful when the list is scrollable.
        assert!(layout.max_scroll as f32 > CRAFTING_SCROLLBAR_MIN_THUMB);
    }

    #[test]
    fn panel_and_row_info_stay_inside_the_panel() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        let screen = [1280.0, 720.0];
        let layout = crafting_layout(screen, &rows, 0);
        let centre = layout.panel.center();
        assert!((centre.0 - 640.0).abs() < 0.01);
        assert!((centre.1 - 360.0).abs() < 0.01);
        let content_bottom = layout.content.y + layout.content.h;
        let mut visible = 0;
        for row in &layout.rows {
            assert!(row.rect.x >= layout.panel.x);
            assert!(row.rect.x + row.rect.w <= layout.panel.x + layout.panel.w);
            if row.rect.y >= layout.content.y && row.rect.y + row.rect.h <= content_bottom {
                visible += 1;
            }
        }
        assert!(visible > 0, "at least one row must be fully visible");
        // Every laid-out row overlaps the viewport or is waiting in the
        // scrollable area above/below it; none may escape horizontally.
        assert!(layout.rows.iter().all(|row| row.rect.x >= layout.content.x));
        // A short screen clamps the panel inside the window.
        let small = crafting_layout([960.0, 540.0], &rows, 0);
        assert!(small.panel.x >= 0.0 && small.panel.y >= 0.0);
        assert!(small.panel.x + small.panel.w <= 960.0);
        assert!(small.panel.y + small.panel.h <= 540.0);
    }

    #[test]
    fn panel_is_620_by_672_with_row_breathing_room() {
        let layout = crafting_panel_rect([1280.0, 720.0]);
        assert_eq!((layout.w, layout.h), (620.0, 672.0));
        for screen in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            let panel = crafting_panel_rect(screen);
            assert!(panel.w <= screen[0] && panel.h <= screen[1]);
            assert!(CRAFTING_ROW_HEIGHT >= ob_render::line_height(1.25) + 4.0);
            assert!(CRAFTING_SECTION_HEIGHT >= ob_render::line_height(1.15) + 8.0);
        }
    }

    #[test]
    fn basics_building_tools_and_armor_sections_have_a_visible_gap() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        let layout = crafting_layout([1280.0, 720.0], &rows, 0);
        let changes: Vec<usize> = rows
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| pair[0].section != pair[1].section)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(changes.len(), 3, "Basics, Building, Tools and Armor");
        for change in changes {
            let previous = layout.rows[change].rect;
            let next = layout.rows[change + 1].rect;
            assert!(
                next.y - (previous.y + previous.h) >= CRAFTING_SECTION_HEIGHT - 0.01,
                "section gap after {} must be visible",
                rows[change].section.title()
            );
        }
        let titles: Vec<&str> = CraftSection::ALL.iter().map(|section| section.title()).collect();
        assert_eq!(titles, ["Basics", "Building", "Tools", "Armor"]);
    }

    #[test]
    fn short_screens_scroll_but_keep_every_row_reachable() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        let screen = [960.0, 540.0];
        let mut state = CraftingState::default();
        let layout = crafting_layout(screen, &rows, 0);
        assert!(layout.max_scroll > 0, "the taller spaced list scrolls at 540p");
        assert!(layout.panel.h <= screen[1]);
        for _ in 0..rows.len() {
            state.move_selection(1, rows.len());
            state.ensure_visible(screen, &rows);
            let layout = crafting_layout(screen, &rows, state.scroll);
            let rect = layout.rows[state.selected].rect;
            assert!(
                rect.y >= layout.content.y - 0.01 && rect.y + rect.h <= layout.content.y + layout.content.h + 0.01,
                "row {} must be reachable at scroll {}",
                state.selected,
                state.scroll
            );
        }
    }

    #[test]
    fn crafting_screen_draws_gated_and_scrollable_panels() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), false, false, &[]);
        assert_eq!(
            rows.iter().filter(|row| row.status == CraftStatus::NeedsTable).count(),
            22
        );
        let screen = [1280.0, 720.0];
        let layout = crafting_layout(screen, &rows, 0);
        let mut state = CraftingState {
            selected: 3,
            ..CraftingState::default()
        };
        state.ensure_visible(screen, &rows);
        let mut frame = UiFrame::new(screen[0], screen[1]);
        draw_crafting_screen(&mut frame, &state, &rows, &registry, false, false, screen);
        let (_, quads) = frame.measure();
        assert!(quads > rows.len() * 10, "got {quads} quads");
        // Scrolling to the bottom still draws the Armor section.
        state.scroll = layout.max_scroll;
        let mut frame = UiFrame::new(screen[0], screen[1]);
        draw_crafting_screen(&mut frame, &state, &rows, &registry, false, false, screen);
        assert!(frame.measure().1 > 0);
        // Creative mode bypasses both ingredients and table gating.
        let creative = crafting_rows(&registry, &[], false, true, &[]);
        assert!(creative.iter().all(|row| row.status == CraftStatus::Creative));
        let mut frame = UiFrame::new(screen[0], screen[1]);
        draw_crafting_screen(&mut frame, &state, &creative, &registry, false, true, screen);
        assert!(frame.measure().1 > 0);
    }

    #[test]
    fn tool_info_reports_durability_and_mining_speed() {
        let registry = Registry::base();
        let rows = crafting_rows(&registry, &all_available(&registry), true, false, &[]);
        let pick = rows
            .iter()
            .find(|row| row.output_name == "Stone Pickaxe")
            .expect("stone pickaxe row");
        let info = pick.output_info(&registry);
        assert!(info.contains("Durability 131/131"), "got {info}");
        assert!(info.contains("Speed 4.0"), "got {info}");
        let planks = &rows[0];
        assert!(planks.output_info(&registry).starts_with("hardness "));
    }
}

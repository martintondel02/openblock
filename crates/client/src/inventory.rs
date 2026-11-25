//! Creative inventory model: registry-backed block list, responsive grid,
//! hover/selection state, hotbar assignment and drawing.
//!
//! The layout and state-machine code is GPU-free and unit-testable; only
//! [`draw`] touches the immediate-mode [`UiFrame`]. The block list always comes
//! from the session [`Registry`], so data packs and future block additions show
//! up without client changes.

use ob_content::{ArmorSlot, BlockId, ItemId, Registry, ToolKind};
use ob_render::UiFrame;
use ob_server::{GameMode, ItemStack};
use ob_sim::MAX_HUNGER;

use crate::ui_state::{BLOCK_NAME_SECONDS, HOTBAR_SLOTS, UiRect, UiState};

/// Number of columns in the inventory grid.
pub const INVENTORY_COLUMNS: usize = 9;
/// Preferred slot edge length in UI pixels.
pub const INVENTORY_SLOT_SIZE: f32 = 48.0;
/// Gap between inventory slots.
pub const INVENTORY_GAP: f32 = 8.0;
/// Horizontal padding inside the inventory panel.
pub const INVENTORY_PADDING: f32 = 26.0;
/// Space reserved for the title above the grid.
pub const INVENTORY_TITLE_HEIGHT: f32 = 56.0;
/// Vertical gap between the grid and the footer separator.
pub const INVENTORY_GRID_GAP: f32 = 16.0;
/// Space reserved for the hovered-name and crafting-hint footer below the grid.
pub const INVENTORY_FOOTER_HEIGHT: f32 = 124.0;
/// Minimum distance between the panel and the screen edge.
pub const INVENTORY_MARGIN: f32 = 24.0;
/// Horizontal padding inside the survival inventory panel.
pub const SURVIVAL_PADDING: f32 = 26.0;
/// Y offsets of the five footer lines, measured from the footer separator.
pub const INVENTORY_FOOTER_LINES: [f32; 5] = [12.0, 36.0, 60.0, 82.0, 102.0];
/// Control hints on the last two footer lines (kept short enough for 960x540).
pub const INVENTORY_CONTROL_HINTS: [&str; 2] = [
    "Click armor equip/unequip   Click/Enter select",
    "Arrows/Wheel navigate   C crafting   E/Esc close",
];
/// Y offsets of the three creative-inventory footer lines.
pub const INVENTORY_CREATIVE_FOOTER_LINES: [f32; 3] = [12.0, 40.0, 68.0];
/// Smallest slot edge length used on very narrow windows.
const MIN_SLOT_SIZE: f32 = 8.0;
/// Width of the one-row survival inventory grid, in items.
const SURVIVAL_COLUMNS: usize = HOTBAR_SLOTS;
/// Edge length of one survival armor slot in UI pixels.
pub const ARMOR_SLOT_SIZE: f32 = 40.0;
/// Vertical space reserved above the hotbar row for armor slots and label.
pub const ARMOR_ROW_HEIGHT: f32 = 64.0;
/// Gap between the armor row and the hotbar row.
pub const ARMOR_ROW_GAP: f32 = 14.0;

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
    /// Y of the footer separator; drawing and spacing tests share it.
    pub footer_top: f32,
    /// Bottom edge of the last visible grid row.
    pub grid_bottom: f32,
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
    /// Armor slot under the mouse cursor in the survival inventory, if any.
    pub armor_hovered: Option<ArmorSlot>,
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
    /// Four armor slots in helmet, chestplate, leggings, boots order.
    pub armor_slots: Vec<UiRect>,
    /// Y of the armor slot row.
    pub armor_top: f32,
    /// Slot edge length after responsive shrinking.
    pub slot_size: f32,
    /// Y of the footer separator; drawing and spacing tests share it.
    pub footer_top: f32,
    /// Bottom edge of the hotbar-slot row.
    pub grid_bottom: f32,
}

impl SurvivalInventoryLayout {
    /// Maps a UI point to a hotbar slot, or `None` outside every slot.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        self.slots.iter().position(|slot| slot.contains(x, y))
    }

    /// Maps a UI point to an armor slot, or `None` outside every armor slot.
    pub fn armor_hit_test(&self, x: f32, y: f32) -> Option<ArmorSlot> {
        const SLOTS: [ArmorSlot; 4] = [
            ArmorSlot::Helmet,
            ArmorSlot::Chestplate,
            ArmorSlot::Leggings,
            ArmorSlot::Boots,
        ];
        self.armor_slots
            .iter()
            .position(|slot| slot.contains(x, y))
            .and_then(|index| SLOTS.get(index).copied())
    }
}

/// Icon shown in a survival hotbar or inventory slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotbarIcon {
    /// A block item drawn as an isometric cube from its top/side texture layers.
    ///
    /// Blocks that declare an interim `+Z` front texture use that texture for
    /// the visible cube sides, so a furnace shows its furnace front.
    Block {
        /// Top texture-array layer.
        top: u32,
        /// Side texture-array layer (`front` when the block declares one).
        side: u32,
    },
    /// The procedural apple icon.
    Apple,
    /// The procedural stick icon.
    Stick,
    /// A procedural pickaxe/axe/shovel/sword icon tinted by tier.
    Tool {
        /// Which tool silhouette to draw.
        kind: ToolKind,
        /// Harvest tier; level 1 is wooden brown, level 2 stone grey, level 3+
        /// iron steel.
        tier: u8,
    },
    /// A coal or charcoal lump.
    Fuel,
    /// Raw metal nuggets in a rocky matrix.
    RawOre(OreKind),
    /// A smelted metal bar.
    Ingot(IngotKind),
    /// A leather or iron armor silhouette.
    Armor {
        /// Which armor piece to draw.
        slot: ArmorSlot,
        /// Leather brown or iron grey.
        material: ArmorMaterial,
    },
    /// A tan leather patch.
    Leather,
    /// A raw or cooked meat cut.
    Meat {
        /// Beef or porkchop silhouette.
        kind: MeatKind,
        /// Cooked cuts use darker, browner colours.
        cooked: bool,
    },
}

/// Metal family for raw ore nuggets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OreKind {
    /// Beige/orange iron nuggets.
    Iron,
    /// Orange/teal copper nuggets.
    Copper,
}

/// Metal family for smelted ingot bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngotKind {
    /// Grey iron bar.
    Iron,
    /// Orange copper bar.
    Copper,
}

/// Material family for armor silhouettes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmorMaterial {
    /// Brown leather armor.
    Leather,
    /// Grey iron armor.
    Iron,
}

/// Animal silhouette for meat icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeatKind {
    /// A marbled beef cut.
    Beef,
    /// A porkchop with a bone edge.
    Porkchop,
}

/// One coloured rectangle of a procedural item icon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconPart {
    /// Drawable rectangle.
    pub rect: UiRect,
    /// Straight-alpha colour.
    pub color: [f32; 4],
}

/// Width of the pixel-art grids used by stick and tool icons.
pub const ITEM_ICON_GRID: usize = 12;
/// Height of a tool durability bar in UI pixels.
pub const DURABILITY_BAR_HEIGHT: f32 = 2.0;
/// Inset of the durability bar from the slot's left and right edges.
pub const DURABILITY_BAR_INSET: f32 = 3.0;
/// Vertical distance from the slot's bottom edge to the durability bar.
pub const DURABILITY_BAR_BOTTOM: f32 = 6.0;

/// Light wood colour of a tool handle.
const HANDLE_LIGHT: [f32; 4] = [0.61, 0.42, 0.21, 1.0];
/// Dark wood colour of a tool handle.
const HANDLE_DARK: [f32; 4] = [0.42, 0.27, 0.13, 1.0];
/// Wooden tool head.
const WOOD_HEAD: [f32; 4] = [0.67, 0.47, 0.25, 1.0];
/// Wooden tool head highlight.
const WOOD_HEAD_LIGHT: [f32; 4] = [0.82, 0.63, 0.38, 1.0];
/// Stone tool head.
const STONE_HEAD: [f32; 4] = [0.54, 0.55, 0.58, 1.0];
/// Stone tool head highlight.
const STONE_HEAD_LIGHT: [f32; 4] = [0.75, 0.76, 0.79, 1.0];
/// Iron tool head.
const IRON_HEAD: [f32; 4] = [0.70, 0.73, 0.78, 1.0];
/// Iron tool head highlight.
const IRON_HEAD_LIGHT: [f32; 4] = [0.89, 0.92, 0.96, 1.0];
/// Coal and charcoal body.
const FUEL_DARK: [f32; 4] = [0.10, 0.10, 0.12, 1.0];
/// Coal and charcoal facet highlight.
const FUEL_LIGHT: [f32; 4] = [0.31, 0.31, 0.35, 1.0];
/// Iron raw nugget.
const RAW_IRON: [f32; 4] = [0.76, 0.66, 0.50, 1.0];
/// Iron raw nugget highlight.
const RAW_IRON_LIGHT: [f32; 4] = [0.93, 0.83, 0.64, 1.0];
/// Copper raw nugget.
const RAW_COPPER: [f32; 4] = [0.82, 0.48, 0.24, 1.0];
/// Copper raw nugget highlight.
const RAW_COPPER_LIGHT: [f32; 4] = [0.97, 0.69, 0.42, 1.0];
/// Iron ingot body.
const IRON_INGOT: [f32; 4] = [0.62, 0.65, 0.69, 1.0];
/// Iron ingot highlight.
const IRON_INGOT_LIGHT: [f32; 4] = [0.88, 0.91, 0.95, 1.0];
/// Copper ingot body.
const COPPER_INGOT: [f32; 4] = [0.79, 0.45, 0.22, 1.0];
/// Copper ingot highlight.
const COPPER_INGOT_LIGHT: [f32; 4] = [0.97, 0.67, 0.40, 1.0];
/// Leather body.
const LEATHER: [f32; 4] = [0.64, 0.44, 0.23, 1.0];
/// Leather highlight.
const LEATHER_LIGHT: [f32; 4] = [0.81, 0.61, 0.35, 1.0];
/// Raw beef body.
const RAW_BEEF: [f32; 4] = [0.71, 0.19, 0.21, 1.0];
/// Raw beef fat/marbling.
const RAW_BEEF_LIGHT: [f32; 4] = [0.94, 0.70, 0.62, 1.0];
/// Cooked beef body.
const COOKED_BEEF: [f32; 4] = [0.45, 0.22, 0.14, 1.0];
/// Cooked beef sear highlight.
const COOKED_BEEF_LIGHT: [f32; 4] = [0.71, 0.46, 0.27, 1.0];
/// Raw porkchop body.
const RAW_PORK: [f32; 4] = [0.84, 0.44, 0.41, 1.0];
/// Raw porkchop fat.
const RAW_PORK_LIGHT: [f32; 4] = [0.97, 0.72, 0.67, 1.0];
/// Cooked porkchop body.
const COOKED_PORK: [f32; 4] = [0.52, 0.27, 0.17, 1.0];
/// Cooked porkchop sear highlight.
const COOKED_PORK_LIGHT: [f32; 4] = [0.73, 0.49, 0.30, 1.0];

/// 12x12 pickaxe silhouette: `+` highlight, `#` head, `h` handle, `H` dark handle.
const PICKAXE_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "..++++++....",
    ".+######+...",
    ".##....##...",
    ".......hh...",
    "......hh....",
    ".....hh.....",
    "....hh......",
    "...hh.......",
    "..hh........",
    ".hh.........",
    ".H..........",
];

/// 12x12 axe silhouette.
const AXE_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "......+++...",
    ".....+###+..",
    ".....#####..",
    "....+#####..",
    "....####hh..",
    ".......hh...",
    "......hh....",
    ".....hh.....",
    "....hh......",
    "...hh.......",
    "..H.........",
];

/// 12x12 shovel silhouette.
const SHOVEL_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    ".....++++...",
    "....+####+..",
    "....######..",
    "....######..",
    "....+####+..",
    ".......hh...",
    "......hh....",
    ".....hh.....",
    "....hh......",
    "...hh.......",
    "..H.........",
];

/// 12x12 sword silhouette.
const SWORD_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    ".......++...",
    "......++#...",
    ".....++#....",
    "....++#.....",
    "...++#......",
    "..++#.......",
    ".#####......",
    "..hh........",
    "..hh........",
    "..h.........",
    "............",
];

/// 12x12 stick silhouette.
const STICK_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    ".........hh.",
    "........hh..",
    ".......hh...",
    "......hh....",
    ".....hh.....",
    "....hh......",
    "...hh.......",
    "..hh........",
    ".hh.........",
    ".h..........",
    "............",
];

/// 12x12 coal/charcoal lump silhouette.
const COAL_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "....###.....",
    "...#####....",
    "..###.####..",
    "..##...###..",
    ".###..####..",
    ".####.##....",
    "..######....",
    "...####.....",
    "....##......",
    "............",
    "............",
];

/// 12x12 raw-metal nugget silhouette.
const RAW_ORE_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    ".....#......",
    "....#+#.....",
    "...##+##....",
    "..###.###...",
    "..##..+##...",
    ".###..####..",
    ".##+#####...",
    "..######....",
    "...####.....",
    "............",
    "............",
];

/// 12x12 smelted-ingot bar silhouette.
const INGOT_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "............",
    "..++++++++..",
    ".+########+.",
    ".#++++++++#.",
    ".#+######+#.",
    "..########..",
    "...++++++...",
    "............",
    "............",
    "............",
    "............",
];

/// 12x12 leather patch silhouette.
const LEATHER_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "..#######...",
    ".+#######+..",
    ".#+#####+#..",
    ".##+###+##..",
    ".##+###+##..",
    ".##+###+##..",
    ".+#######+..",
    "..#######...",
    "............",
    "............",
    "............",
];

/// 12x12 beef-cut silhouette.
const BEEF_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "...######...",
    "..########..",
    ".##++##++##.",
    ".##++++++##.",
    ".##++##++##.",
    "..########..",
    "...######...",
    ".....##.....",
    ".....##.....",
    "............",
    "............",
];

/// 12x12 porkchop silhouette.
const PORKCHOP_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "....####....",
    "...######...",
    "..########..",
    "..#++++++#..",
    "..##++++##..",
    "..###++###..",
    "...######...",
    "....####....",
    ".....##.....",
    "............",
    "............",
];

/// 12x12 helmet silhouette.
const HELMET_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "....####....",
    "...######...",
    "..########..",
    "..########..",
    "..##....##..",
    "..##....##..",
    "..########..",
    "............",
    "............",
    "............",
    "............",
];

/// 12x12 chestplate silhouette.
const CHESTPLATE_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "....####....",
    "..########..",
    ".##########.",
    "##..####..##",
    "##..####..##",
    "##..####..##",
    "##..####..##",
    "....####....",
    "....####....",
    "............",
    "............",
];

/// 12x12 leggings silhouette.
const LEGGINGS_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "...######...",
    "..########..",
    "..########..",
    "..###..###..",
    "..###..###..",
    "..###..###..",
    "..###..###..",
    "..###..###..",
    "..###..###..",
    "..###..###..",
    "............",
];

/// 12x12 boots silhouette.
const BOOTS_ART: [&str; ITEM_ICON_GRID] = [
    "............",
    "............",
    "..####......",
    "..####..###.",
    "..####..###.",
    "..#####.###.",
    "..#####.###.",
    "..##########",
    "..##########",
    "............",
    "............",
    "............",
];

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
        self.armor_hovered = None;
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

/// Safe display name for an item id; out-of-range ids fall back to
/// `Unknown item`.
pub fn item_display_name(registry: &Registry, item: ItemId) -> String {
    if item.0 as usize >= registry.item_len() {
        return "Unknown item".to_string();
    }
    display_name(registry.item(item).id)
}

/// Human-readable armor slot label used by the info line.
pub fn armor_slot_label(slot: ArmorSlot) -> &'static str {
    match slot {
        ArmorSlot::Helmet => "Helmet",
        ArmorSlot::Chestplate => "Chestplate",
        ArmorSlot::Leggings => "Leggings",
        ArmorSlot::Boots => "Boots",
    }
}

/// One-line item hint shared by the survival inventory and crafting footer.
///
/// Tools report durability and speed, armor reports its slot/defence/durability,
/// food reports hunger, coal/charcoal report furnace burn ticks, block items
/// report hardness and other crafting materials say so. Unknown ids fall back
/// safely instead of panicking.
pub fn item_info_hint(registry: &Registry, item: ItemId, durability: Option<u16>) -> String {
    if item.0 as usize >= registry.item_len() {
        return "Unknown item".to_string();
    }
    let item_type = registry.item(item);
    if let Some(spec) = item_type.tool {
        let remaining = durability.unwrap_or(spec.durability).min(spec.durability);
        return format!(
            "{} tier {} · Durability {remaining}/{} · Speed {:.1}",
            tool_kind_label(spec.kind),
            spec.tier,
            spec.durability,
            spec.speed
        );
    }
    if let Some(spec) = item_type.armor {
        let remaining = durability.unwrap_or(spec.durability).min(spec.durability);
        return format!(
            "{} · Defence {} · Durability {remaining}/{}",
            armor_slot_label(spec.slot),
            spec.defense,
            spec.durability
        );
    }
    if item_type.edible() {
        return format!("Food +{} hunger", item_type.food.unwrap_or(0));
    }
    if matches!(item_type.id, "openblock:coal" | "openblock:charcoal")
        && let Some(ticks) = registry.fuel_ticks(item)
    {
        return format!("Fuel ({ticks} ticks)");
    }
    if let Some(block) = item_type.block {
        return hardness_hint(registry.block(block).hardness);
    }
    match item_type.id {
        "openblock:stick" | "openblock:leather" | "openblock:iron_ingot" | "openblock:copper_ingot" => {
            "Crafting material".to_string()
        }
        "openblock:raw_iron" => "Smelt in a furnace: Iron Ingot".to_string(),
        "openblock:raw_copper" => "Smelt in a furnace: Copper Ingot".to_string(),
        _ => "Crafting material".to_string(),
    }
}

/// Item name and hint shown in the survival inventory footer.
pub fn survival_item_info(registry: &Registry, stack: Option<ItemStack>) -> (String, String) {
    let Some(stack) = stack.filter(|stack| stack.count > 0) else {
        return ("Empty slot".to_string(), "No item".to_string());
    };
    let name = item_display_name(registry, stack.item);
    let hint = item_info_hint(registry, stack.item, stack.durability);
    (name, hint)
}

/// Title-cased tool kind used by inventory and F3 hints.
pub fn tool_kind_label(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Pickaxe => "Pickaxe",
        ToolKind::Axe => "Axe",
        ToolKind::Shovel => "Shovel",
        ToolKind::Sword => "Sword",
    }
}

/// Durability bar fill fraction in `0.0..=1.0`.
///
/// A zero or missing maximum reports an empty bar; a remaining value above the
/// maximum is clamped so a malformed save cannot draw outside the slot.
pub fn durability_fraction(remaining: u16, max: u16) -> f32 {
    if max == 0 {
        return 0.0;
    }
    (f32::from(remaining.min(max)) / f32::from(max)).clamp(0.0, 1.0)
}

/// Colour of a durability bar at `fraction`: green, amber then red.
pub fn durability_color(fraction: f32) -> [f32; 4] {
    if !fraction.is_finite() {
        return [0.85, 0.28, 0.22, 1.0];
    }
    let fraction = fraction.clamp(0.0, 1.0);
    if fraction > 0.5 {
        [0.45, 0.78, 0.30, 1.0]
    } else if fraction > 0.2 {
        [0.92, 0.75, 0.25, 1.0]
    } else {
        [0.86, 0.28, 0.22, 1.0]
    }
}

/// Durability of a stack and its maximum, if it is a tool or armor piece.
pub fn stack_durability(registry: &Registry, stack: ItemStack) -> Option<(u16, u16)> {
    if stack.item.0 as usize >= registry.item_len() {
        return None;
    }
    let item = registry.item(stack.item);
    if let Some(spec) = item.tool {
        let remaining = stack.durability.unwrap_or(spec.durability).min(spec.durability);
        return Some((remaining, spec.durability));
    }
    if let Some(spec) = item.armor {
        let remaining = stack.durability.unwrap_or(spec.durability).min(spec.durability);
        return Some((remaining, spec.durability));
    }
    None
}

/// Draws the thin durability bar along the bottom edge of a slot.
pub fn draw_durability_bar(frame: &mut UiFrame, slot: UiRect, remaining: u16, max: u16) {
    if max == 0 || slot.w <= DURABILITY_BAR_INSET * 2.0 || slot.h <= DURABILITY_BAR_BOTTOM {
        return;
    }
    let fraction = durability_fraction(remaining, max);
    let x = slot.x + DURABILITY_BAR_INSET;
    let width = slot.w - DURABILITY_BAR_INSET * 2.0;
    let y = slot.y + slot.h - DURABILITY_BAR_BOTTOM;
    frame.rect(
        x - 1.0,
        y - 1.0,
        width + 2.0,
        DURABILITY_BAR_HEIGHT + 2.0,
        [0.02, 0.02, 0.03, 0.9],
    );
    frame.rect(x, y, width, DURABILITY_BAR_HEIGHT, [0.18, 0.19, 0.22, 1.0]);
    if fraction > 0.0 {
        frame.rect(
            x,
            y,
            width * fraction,
            DURABILITY_BAR_HEIGHT,
            durability_color(fraction),
        );
    }
}

/// Draws a stack's durability bar when it is a tool.
pub fn draw_stack_durability(frame: &mut UiFrame, slot: UiRect, registry: &Registry, stack: ItemStack) {
    if let Some((remaining, max)) = stack_durability(registry, stack)
        && remaining < max
    {
        // Undamaged gear keeps a clean icon; a full green bar on every tool
        // looked like a selection/charge indicator.
        draw_durability_bar(frame, slot, remaining, max);
    }
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
    let panel_height = INVENTORY_TITLE_HEIGHT
        + ARMOR_ROW_HEIGHT
        + ARMOR_ROW_GAP
        + slot_size
        + INVENTORY_GRID_GAP
        + INVENTORY_FOOTER_HEIGHT
        // The armour row starts 10 px lower to clear the header rule.
        + 10.0;
    let panel = UiRect::new(
        (width - panel_width) / 2.0,
        (height - panel_height) / 2.0,
        panel_width,
        panel_height,
    );
    let grid_x = panel.x + SURVIVAL_PADDING;
    let armor_top = panel.y + INVENTORY_TITLE_HEIGHT + 30.0;
    let armor_gap = 8.0;
    let armor_width = 4.0 * ARMOR_SLOT_SIZE + 3.0 * armor_gap;
    let armor_x = panel.x + (panel.w - armor_width) / 2.0;
    let armor_slots = (0..4)
        .map(|index| {
            UiRect::new(
                armor_x + index as f32 * (ARMOR_SLOT_SIZE + armor_gap),
                armor_top,
                ARMOR_SLOT_SIZE,
                ARMOR_SLOT_SIZE,
            )
        })
        .collect();
    let grid_y = armor_top + ARMOR_SLOT_SIZE + ARMOR_ROW_GAP;
    let slots = (0..SURVIVAL_COLUMNS)
        .map(|index| UiRect::new(grid_x + index as f32 * (slot_size + gap), grid_y, slot_size, slot_size))
        .collect();
    SurvivalInventoryLayout {
        panel,
        slots,
        armor_slots,
        armor_top,
        slot_size,
        footer_top: panel.y + panel.h - INVENTORY_FOOTER_HEIGHT,
        grid_bottom: grid_y + slot_size,
    }
}

/// Maps a UI point to a survival hotbar slot.
pub fn survival_hit_test(screen: [f32; 2], x: f32, y: f32) -> Option<usize> {
    survival_layout(screen).hit_test(x, y)
}

/// Maps a UI point to one of the four survival armor slots.
pub fn survival_armor_hit_test(screen: [f32; 2], x: f32, y: f32) -> Option<ArmorSlot> {
    survival_layout(screen).armor_hit_test(x, y)
}

/// Equipment-array index of an armor slot (helmet, chestplate, leggings, boots).
pub fn armor_index(slot: ArmorSlot) -> usize {
    match slot {
        ArmorSlot::Helmet => 0,
        ArmorSlot::Chestplate => 1,
        ArmorSlot::Leggings => 2,
        ArmorSlot::Boots => 3,
    }
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

/// Icon for an inventory stack: block items use their cube icon, and the
/// procedural apple/stick/tool shapes cover the interim non-block items.
/// Empty slots and unknown non-block items have no icon.
pub fn item_icon(registry: &Registry, stack: Option<ItemStack>) -> Option<HotbarIcon> {
    let stack = stack.filter(|stack| stack.count > 0)?;
    item_icon_for(registry, stack.item)
}

/// Icon for an item id, without needing a stack (crafting rows use this).
///
/// Every block item uses its isometric block icon; every non-block item in the
/// base catalogue has a procedural icon. Out-of-range ids return `None`.
pub fn item_icon_for(registry: &Registry, item: ItemId) -> Option<HotbarIcon> {
    if item.0 as usize >= registry.item_len() {
        return None;
    }
    let item_type = registry.item(item);
    if let Some(block) = item_type.block {
        let block = registry.block(block);
        return Some(HotbarIcon::Block {
            top: u32::from(block.textures.top.0),
            // Fronted blocks (furnace, chest) show their front on the cube
            // sides; the render helper only accepts one side texture.
            side: u32::from(block.textures.front_texture().0),
        });
    }
    if let Some(spec) = item_type.tool {
        return Some(HotbarIcon::Tool {
            kind: spec.kind,
            tier: spec.tier,
        });
    }
    if let Some(spec) = item_type.armor {
        let material = if item_type.id.starts_with("openblock:leather") {
            ArmorMaterial::Leather
        } else {
            ArmorMaterial::Iron
        };
        return Some(HotbarIcon::Armor {
            slot: spec.slot,
            material,
        });
    }
    match item_type.id {
        "openblock:apple" => Some(HotbarIcon::Apple),
        "openblock:stick" => Some(HotbarIcon::Stick),
        "openblock:coal" | "openblock:charcoal" => Some(HotbarIcon::Fuel),
        "openblock:raw_iron" => Some(HotbarIcon::RawOre(OreKind::Iron)),
        "openblock:raw_copper" => Some(HotbarIcon::RawOre(OreKind::Copper)),
        "openblock:iron_ingot" => Some(HotbarIcon::Ingot(IngotKind::Iron)),
        "openblock:copper_ingot" => Some(HotbarIcon::Ingot(IngotKind::Copper)),
        "openblock:leather" => Some(HotbarIcon::Leather),
        "openblock:raw_beef" => Some(HotbarIcon::Meat {
            kind: MeatKind::Beef,
            cooked: false,
        }),
        "openblock:cooked_beef" => Some(HotbarIcon::Meat {
            kind: MeatKind::Beef,
            cooked: true,
        }),
        "openblock:raw_porkchop" => Some(HotbarIcon::Meat {
            kind: MeatKind::Porkchop,
            cooked: false,
        }),
        "openblock:cooked_porkchop" => Some(HotbarIcon::Meat {
            kind: MeatKind::Porkchop,
            cooked: true,
        }),
        _ => None,
    }
}

/// Builds coloured rectangles from one ASCII pixel-art grid.
fn grid_parts(
    art: &[&str; ITEM_ICON_GRID],
    x: f32,
    y: f32,
    size: f32,
    color_for: impl Fn(char) -> Option<[f32; 4]>,
) -> Vec<IconPart> {
    let mut parts = Vec::new();
    let size = size.max(0.0);
    if !x.is_finite() || !y.is_finite() || size <= 0.0 {
        return parts;
    }
    let unit = size / ITEM_ICON_GRID as f32;
    for (row, line) in art.iter().enumerate() {
        for (column, glyph) in line.chars().enumerate() {
            let Some(color) = color_for(glyph) else {
                continue;
            };
            if column >= ITEM_ICON_GRID {
                break;
            }
            parts.push(IconPart {
                rect: UiRect::new(x + column as f32 * unit, y + row as f32 * unit, unit, unit),
                color,
            });
        }
    }
    parts
}

/// Colours of a tool head for a tier: wood for tier 1, stone for tier 2 and
/// steel for iron tier 3+.
fn tool_head_colors(tier: u8) -> ([f32; 4], [f32; 4]) {
    match tier {
        0 | 1 => (WOOD_HEAD, WOOD_HEAD_LIGHT),
        2 => (STONE_HEAD, STONE_HEAD_LIGHT),
        _ => (IRON_HEAD, IRON_HEAD_LIGHT),
    }
}

fn tool_art(kind: ToolKind) -> [&'static str; ITEM_ICON_GRID] {
    match kind {
        ToolKind::Pickaxe => PICKAXE_ART,
        ToolKind::Axe => AXE_ART,
        ToolKind::Shovel => SHOVEL_ART,
        ToolKind::Sword => SWORD_ART,
    }
}

/// Procedural stick icon geometry, in draw order.
pub fn stick_icon_parts(x: f32, y: f32, size: f32) -> Vec<IconPart> {
    grid_parts(&STICK_ART, x, y, size, |glyph| match glyph {
        'h' => Some(HANDLE_LIGHT),
        'H' => Some(HANDLE_DARK),
        _ => None,
    })
}

/// Procedural tool icon geometry, in draw order.
///
/// The handle is always wood brown; the head is wood brown for tier 1, grey for
/// tier 2 and steel grey-blue for iron tier 3+. The art is authored on a 12×12
/// grid and scaled to `size`.
pub fn tool_icon_parts(kind: ToolKind, tier: u8, x: f32, y: f32, size: f32) -> Vec<IconPart> {
    let (head, head_light) = tool_head_colors(tier);
    grid_parts(&tool_art(kind), x, y, size, |glyph| match glyph {
        '#' => Some(head),
        '+' => Some(head_light),
        'h' => Some(HANDLE_LIGHT),
        'H' => Some(HANDLE_DARK),
        _ => None,
    })
}

/// Procedural coal/charcoal lump icon geometry.
pub fn fuel_icon_parts(x: f32, y: f32, size: f32) -> Vec<IconPart> {
    grid_parts(&COAL_ART, x, y, size, |glyph| match glyph {
        '#' => Some(FUEL_DARK),
        '+' => Some(FUEL_LIGHT),
        _ => None,
    })
}

/// Procedural raw-metal nugget icon geometry.
pub fn raw_ore_icon_parts(kind: OreKind, x: f32, y: f32, size: f32) -> Vec<IconPart> {
    let (body, light) = match kind {
        OreKind::Iron => (RAW_IRON, RAW_IRON_LIGHT),
        OreKind::Copper => (RAW_COPPER, RAW_COPPER_LIGHT),
    };
    grid_parts(&RAW_ORE_ART, x, y, size, |glyph| match glyph {
        '#' => Some(body),
        '+' => Some(light),
        _ => None,
    })
}

/// Procedural smelted-ingot icon geometry.
pub fn ingot_icon_parts(kind: IngotKind, x: f32, y: f32, size: f32) -> Vec<IconPart> {
    let (body, light) = match kind {
        IngotKind::Iron => (IRON_INGOT, IRON_INGOT_LIGHT),
        IngotKind::Copper => (COPPER_INGOT, COPPER_INGOT_LIGHT),
    };
    grid_parts(&INGOT_ART, x, y, size, |glyph| match glyph {
        '#' => Some(body),
        '+' => Some(light),
        _ => None,
    })
}

/// Procedural leather patch icon geometry.
pub fn leather_icon_parts(x: f32, y: f32, size: f32) -> Vec<IconPart> {
    grid_parts(&LEATHER_ART, x, y, size, |glyph| match glyph {
        '#' => Some(LEATHER),
        '+' => Some(LEATHER_LIGHT),
        _ => None,
    })
}

fn armor_material_colors(material: ArmorMaterial) -> ([f32; 4], [f32; 4]) {
    match material {
        ArmorMaterial::Leather => (LEATHER, LEATHER_LIGHT),
        ArmorMaterial::Iron => (IRON_INGOT, IRON_INGOT_LIGHT),
    }
}

fn armor_art(slot: ArmorSlot) -> [&'static str; ITEM_ICON_GRID] {
    match slot {
        ArmorSlot::Helmet => HELMET_ART,
        ArmorSlot::Chestplate => CHESTPLATE_ART,
        ArmorSlot::Leggings => LEGGINGS_ART,
        ArmorSlot::Boots => BOOTS_ART,
    }
}

/// Procedural armor icon geometry: leather brown or iron grey.
pub fn armor_icon_parts(slot: ArmorSlot, material: ArmorMaterial, x: f32, y: f32, size: f32) -> Vec<IconPart> {
    let (body, light) = armor_material_colors(material);
    grid_parts(&armor_art(slot), x, y, size, |glyph| match glyph {
        '#' => Some(body),
        '+' => Some(light),
        _ => None,
    })
}

/// Procedural meat-cut icon geometry; cooked cuts use darker colours.
pub fn meat_icon_parts(kind: MeatKind, cooked: bool, x: f32, y: f32, size: f32) -> Vec<IconPart> {
    let (body, light) = match (kind, cooked) {
        (MeatKind::Beef, false) => (RAW_BEEF, RAW_BEEF_LIGHT),
        (MeatKind::Beef, true) => (COOKED_BEEF, COOKED_BEEF_LIGHT),
        (MeatKind::Porkchop, false) => (RAW_PORK, RAW_PORK_LIGHT),
        (MeatKind::Porkchop, true) => (COOKED_PORK, COOKED_PORK_LIGHT),
    };
    let art = match kind {
        MeatKind::Beef => BEEF_ART,
        MeatKind::Porkchop => PORKCHOP_ART,
    };
    grid_parts(&art, x, y, size, |glyph| match glyph {
        '#' => Some(body),
        '+' => Some(light),
        _ => None,
    })
}

/// Flat representative colour of an item, used by dropped-item entities.
///
/// Block items use their block colour; procedural items report the dominant
/// icon colour. Unknown ids return `None`.
pub fn item_tint_for(registry: &Registry, item: ItemId) -> Option<[f32; 3]> {
    if item.0 as usize >= registry.item_len() {
        return None;
    }
    let item_type = registry.item(item);
    if let Some(block) = item_type.block {
        return Some(registry.block(block).color);
    }
    let color = match item_icon_for(registry, item)? {
        HotbarIcon::Apple => [0.82, 0.14, 0.17, 1.0],
        HotbarIcon::Stick => HANDLE_LIGHT,
        HotbarIcon::Tool { tier, .. } => tool_head_colors(tier).0,
        HotbarIcon::Fuel => FUEL_DARK,
        HotbarIcon::RawOre(OreKind::Iron) => RAW_IRON,
        HotbarIcon::RawOre(OreKind::Copper) => RAW_COPPER,
        HotbarIcon::Ingot(IngotKind::Iron) => IRON_INGOT,
        HotbarIcon::Ingot(IngotKind::Copper) => COPPER_INGOT,
        HotbarIcon::Armor { material, .. } => armor_material_colors(material).0,
        HotbarIcon::Leather => LEATHER,
        HotbarIcon::Meat { kind, cooked } => match (kind, cooked) {
            (MeatKind::Beef, false) => RAW_BEEF,
            (MeatKind::Beef, true) => COOKED_BEEF,
            (MeatKind::Porkchop, false) => RAW_PORK,
            (MeatKind::Porkchop, true) => COOKED_PORK,
        },
        HotbarIcon::Block { .. } => return None,
    };
    Some([color[0], color[1], color[2]])
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

/// Draws an item/block icon; used by the hotbar, survival inventory, crafting
/// rows and pickup summaries.
pub fn draw_item_icon(frame: &mut UiFrame, registry: &Registry, stack: Option<ItemStack>, x: f32, y: f32, size: f32) {
    let Some(icon) = item_icon(registry, stack) else {
        return;
    };
    draw_icon(frame, icon, x, y, size);
}

/// Draws the icon for an item id (crafting outputs have no stack).
pub fn draw_item_icon_for(frame: &mut UiFrame, registry: &Registry, item: ItemId, x: f32, y: f32, size: f32) {
    if let Some(icon) = item_icon_for(registry, item) {
        draw_icon(frame, icon, x, y, size);
    }
}

fn draw_icon(frame: &mut UiFrame, icon: HotbarIcon, x: f32, y: f32, size: f32) {
    match icon {
        HotbarIcon::Block { top, side } => frame.block_icon(x, y, size, top, side),
        HotbarIcon::Apple => draw_apple_icon(frame, x, y, size),
        HotbarIcon::Stick => draw_icon_parts(frame, &stick_icon_parts(x, y, size)),
        HotbarIcon::Tool { kind, tier } => draw_icon_parts(frame, &tool_icon_parts(kind, tier, x, y, size)),
        HotbarIcon::Fuel => draw_icon_parts(frame, &fuel_icon_parts(x, y, size)),
        HotbarIcon::RawOre(kind) => draw_icon_parts(frame, &raw_ore_icon_parts(kind, x, y, size)),
        HotbarIcon::Ingot(kind) => draw_icon_parts(frame, &ingot_icon_parts(kind, x, y, size)),
        HotbarIcon::Armor { slot, material } => {
            draw_icon_parts(frame, &armor_icon_parts(slot, material, x, y, size));
        }
        HotbarIcon::Leather => draw_icon_parts(frame, &leather_icon_parts(x, y, size)),
        HotbarIcon::Meat { kind, cooked } => {
            draw_icon_parts(frame, &meat_icon_parts(kind, cooked, x, y, size));
        }
    }
}

fn draw_icon_parts(frame: &mut UiFrame, parts: &[IconPart]) {
    for part in parts {
        frame.rect(part.rect.x, part.rect.y, part.rect.w, part.rect.h, part.color);
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
        draw_slot_stack(frame, *slot, registry, stack);
    }
}

/// Draws one slot's stack (icon, count and durability bar) inside `slot`.
pub fn draw_slot_stack(frame: &mut UiFrame, slot: UiRect, registry: &Registry, stack: Option<ItemStack>) {
    let Some(stack) = stack.filter(|stack| stack.count > 0) else {
        return;
    };
    draw_item_icon(
        frame,
        registry,
        Some(stack),
        slot.x + 6.0,
        slot.y + 5.0,
        (slot.w - 12.0).max(0.0),
    );
    draw_stack_count(frame, slot, stack.count);
    draw_stack_durability(frame, slot, registry, stack);
}

/// Authoritative host view needed to draw the survival inventory screen.
#[derive(Debug, Clone, Copy)]
pub struct SurvivalInventoryView<'a> {
    /// Nine hotbar stacks.
    pub inventory: &'a [Option<ItemStack>; HOTBAR_SLOTS],
    /// Four equipped armor stacks.
    pub equipment: &'a [Option<ItemStack>; 4],
    /// Total armor defence points.
    pub defense: u8,
    /// Selected hotbar slot.
    pub selected: usize,
    /// Whether a crafting table is nearby.
    pub near_table: bool,
    /// Replacement for the crafting hint line.
    ///
    /// Single-player leaves this `None` and gets the context-sensitive
    /// crafting instructions. The network client has no networked crafting and
    /// passes an explanatory line instead.
    pub craft_hint: Option<&'a str>,
}

/// Draws the survival inventory screen: four armor slots with durability bars
/// and a defence total above the nine-slot hotbar row, plus a footer describing
/// the hovered slot.
pub fn draw_survival(
    frame: &mut UiFrame,
    ui: &UiState,
    registry: &Registry,
    view: &SurvivalInventoryView<'_>,
    screen: [f32; 2],
) {
    let inventory = view.inventory;
    let equipment = view.equipment;
    let defense = view.defense;
    let selected = view.selected;
    let near_table = view.near_table;
    let layout = survival_layout(screen);
    let white = [1.0, 1.0, 1.0, 1.0];
    let muted = [0.78, 0.83, 0.90, 1.0];
    frame.rect(0.0, 0.0, screen[0], screen[1], [0.0, 0.0, 0.0, 0.55]);
    frame.rect(
        layout.panel.x + 6.0,
        layout.panel.y + 8.0,
        layout.panel.w,
        layout.panel.h,
        [0.0, 0.0, 0.0, 0.38],
    );
    frame.rect(
        layout.panel.x,
        layout.panel.y,
        layout.panel.w,
        layout.panel.h,
        [0.055, 0.068, 0.092, 0.985],
    );
    frame.outline(
        layout.panel.x,
        layout.panel.y,
        layout.panel.w,
        layout.panel.h,
        2.0,
        [0.43, 0.49, 0.58, 1.0],
    );
    frame.outline(
        layout.panel.x + 3.0,
        layout.panel.y + 3.0,
        layout.panel.w - 6.0,
        layout.panel.h - 6.0,
        1.0,
        [0.15, 0.18, 0.24, 1.0],
    );
    frame.title_text(
        layout.panel.x + SURVIVAL_PADDING,
        layout.panel.y + 16.0,
        2.6,
        white,
        "Inventory",
    );
    let version = format!("OpenBlock v{}", ob_core::VERSION);
    let version_width = frame.text_width(1.0, &version);
    frame.text(
        layout.panel.x + layout.panel.w - SURVIVAL_PADDING - version_width,
        layout.panel.y + 24.0,
        1.0,
        [0.64, 0.70, 0.79, 1.0],
        &version,
    );
    frame.rect(
        layout.panel.x + SURVIVAL_PADDING,
        layout.panel.y + 62.0,
        layout.panel.w - 2.0 * SURVIVAL_PADDING,
        1.0,
        [0.24, 0.28, 0.35, 1.0],
    );

    // --- Armor row: four equip slots plus the defence total ---
    frame.text(
        layout.armor_slots.first().map_or(layout.panel.x, |slot| slot.x),
        layout.armor_top - 17.0,
        1.2,
        muted,
        "Armor",
    );
    const ARMOR_ORDER: [ArmorSlot; 4] = [
        ArmorSlot::Helmet,
        ArmorSlot::Chestplate,
        ArmorSlot::Leggings,
        ArmorSlot::Boots,
    ];
    const ARMOR_INITIALS: [&str; 4] = ["H", "C", "L", "B"];
    const ARMOR_SHORT_LABELS: [&str; 4] = ["Helm", "Chest", "Legs", "Boots"];
    for (index, slot) in layout.armor_slots.iter().enumerate() {
        let armor_slot = ARMOR_ORDER[index];
        let stack = equipment.get(index).copied().flatten();
        let hovered = ui.inventory.armor_hovered == Some(armor_slot);
        let background = if hovered {
            [0.25, 0.30, 0.38, 0.98]
        } else {
            [0.09, 0.10, 0.14, 0.94]
        };
        frame.rect(slot.x, slot.y, slot.w, slot.h, background);
        frame.outline(
            slot.x,
            slot.y,
            slot.w,
            slot.h,
            if hovered { 2.0 } else { 1.0 },
            if hovered {
                [0.88, 0.92, 0.98, 1.0]
            } else if stack.is_some() {
                [0.55, 0.60, 0.68, 0.95]
            } else {
                [0.28, 0.31, 0.37, 0.75]
            },
        );
        if stack.is_some() {
            draw_slot_stack(frame, *slot, registry, stack);
        } else {
            let label = ARMOR_INITIALS[index];
            let width = frame.text_width(1.2, label);
            frame.text(
                slot.x + (slot.w - width) / 2.0,
                slot.y + slot.h / 2.0 - 6.0,
                1.2,
                [0.42, 0.46, 0.53, 0.9],
                label,
            );
        }
        frame.text(
            slot.x,
            slot.y + slot.h + 2.0,
            0.9,
            [0.68, 0.73, 0.80, 1.0],
            ARMOR_SHORT_LABELS[index],
        );
    }
    let defense_x = layout
        .armor_slots
        .last()
        .map_or(layout.panel.x + layout.panel.w - 60.0, |slot| slot.x + slot.w + 18.0);
    frame.text(
        defense_x,
        layout.armor_top + 4.0,
        1.4,
        if defense > 0 {
            [0.76, 0.86, 0.98, 1.0]
        } else {
            [0.55, 0.60, 0.68, 1.0]
        },
        "Defence",
    );
    frame.text(
        defense_x,
        layout.armor_top + 22.0,
        1.6,
        [0.90, 0.94, 1.0, 1.0],
        &format!("{defense}"),
    );
    frame.text(
        defense_x,
        layout.armor_top + 42.0,
        0.9,
        [0.68, 0.73, 0.80, 1.0],
        "click armor",
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
            draw_stack_durability(frame, *slot, registry, stack);
        }
    }

    let footer_top = layout.footer_top;
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
    let armor_info = ui
        .inventory
        .armor_hovered
        .and_then(|slot| equipment.get(armor_index(slot)).copied().flatten());
    let (name, hint) = if ui.inventory.armor_hovered.is_some() {
        match armor_info {
            Some(stack) => survival_item_info(registry, Some(stack)),
            None => (
                "Empty armor slot".to_string(),
                "Click an armor piece to equip it".to_string(),
            ),
        }
    } else {
        survival_item_info(registry, inventory[info_slot])
    };
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + INVENTORY_FOOTER_LINES[0],
        2.0,
        white,
        &name,
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + INVENTORY_FOOTER_LINES[1],
        1.2,
        muted,
        &hint,
    );
    let craft_hint = view.craft_hint.unwrap_or(if near_table {
        "Crafting table nearby: 3x3   Press C to craft"
    } else {
        "Press C to craft (2x2 hand)   place a table for 3x3"
    });
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + INVENTORY_FOOTER_LINES[2],
        1.0,
        if near_table {
            [0.96, 0.80, 0.42, 1.0]
        } else {
            [0.72, 0.78, 0.86, 1.0]
        },
        craft_hint,
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + INVENTORY_FOOTER_LINES[3],
        1.0,
        [0.80, 0.85, 0.92, 1.0],
        INVENTORY_CONTROL_HINTS[0],
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + INVENTORY_FOOTER_LINES[4],
        1.0,
        [0.80, 0.85, 0.92, 1.0],
        INVENTORY_CONTROL_HINTS[1],
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

    // Below the 720p/540p baseline (for example 960x540 at 150% GUI scale,
    // which is 640x360 logical) leave the gameplay hotbar its band, tighten the
    // title/footer and centre the palette in the remaining space. The survival
    // inventory has its own hotbar row and does not need this reserve.
    let hud_reserve = if height < 480.0 { 56.0 } else { 0.0 };
    let compact = height < 480.0;
    let title_height = if compact { 40.0 } else { INVENTORY_TITLE_HEIGHT };
    let footer_height = if compact { 92.0 } else { INVENTORY_FOOTER_HEIGHT };
    let max_panel_height = (height - 2.0 * INVENTORY_MARGIN - hud_reserve).max(1.0);
    let available_grid_height = (max_panel_height - title_height - footer_height - INVENTORY_GRID_GAP).max(0.0);
    let row_pitch = slot_size + gap;
    let rows_fit = (((available_grid_height + gap) / row_pitch).floor() as usize).max(1);

    let total_rows = item_count.div_ceil(INVENTORY_COLUMNS);
    let visible_rows = total_rows.min(rows_fit);
    let max_scroll = total_rows.saturating_sub(visible_rows);
    let scroll = scroll.min(max_scroll);

    let visible_height = visible_rows as f32 * slot_size + visible_rows.saturating_sub(1) as f32 * gap;
    let panel_height = title_height + visible_height + INVENTORY_GRID_GAP + footer_height;
    let panel = UiRect::new(
        (width - panel_width) / 2.0,
        ((height - hud_reserve - panel_height) / 2.0).max(INVENTORY_MARGIN.min(height / 2.0)),
        panel_width,
        panel_height,
    );
    let grid_x = panel.x + INVENTORY_PADDING;
    let grid_y = panel.y + title_height;

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
        footer_top: panel.y + panel.h - footer_height,
        grid_bottom: grid_y + visible_height,
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
        layout.panel.y + 18.0,
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
            if let Some(item) = registry.item_for_block(items[slot.index]) {
                draw_item_icon_for(frame, registry, item, slot.rect.x + 6.0, slot.rect.y + 5.0, icon_size);
            } else {
                frame.block_icon(
                    slot.rect.x + 6.0,
                    slot.rect.y + 5.0,
                    icon_size,
                    u32::from(block.textures.top.0),
                    u32::from(block.textures.front_texture().0),
                );
            }
        }
    }

    let footer_top = layout.footer_top;
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
        footer_top + INVENTORY_CREATIVE_FOOTER_LINES[0],
        2.0,
        white,
        &name,
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + INVENTORY_CREATIVE_FOOTER_LINES[1],
        1.2,
        muted,
        &hint,
    );
    frame.text_centered(
        layout.panel.x + layout.panel.w / 2.0,
        footer_top + INVENTORY_CREATIVE_FOOTER_LINES[2],
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
        assert_eq!(items.len(), 19);
        assert_eq!(registry.len(), 20);
        assert!(!items.contains(&BlockId::AIR));
        for name in [
            "openblock:stone",
            "openblock:snow",
            "openblock:gravel",
            "openblock:clay",
            "openblock:planks",
            "openblock:cobblestone",
            "openblock:crafting_table",
            "openblock:coal_ore",
            "openblock:iron_ore",
            "openblock:copper_ore",
            "openblock:furnace",
            "openblock:chest",
        ] {
            assert!(
                items.iter().any(|id| registry.block(*id).id == name),
                "{name} must appear in the creative inventory"
            );
        }
    }

    #[test]
    fn layout_is_centred_with_nine_columns_and_slots_inside_the_panel() {
        let screen = [1280.0, 720.0];
        let layout = layout(screen, 19, 0);
        assert_eq!(layout.slots.len(), 19);
        assert_eq!(layout.total_rows, 3);
        assert_eq!(layout.visible_rows, 3);
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
        let layout = layout(screen, 19, 0);
        let slot = layout.slots[3];
        assert_eq!(layout.hit_test(slot.rect.center().0, slot.rect.center().1), Some(3));
        let slot = layout.slots[10];
        assert_eq!(layout.hit_test(slot.rect.center().0, slot.rect.center().1), Some(10));
        assert_eq!(layout.hit_test(1.0, 1.0), None);
        assert_eq!(layout.slot_rect(10), Some(layout.slots[10].rect));
        assert_eq!(layout.slot_rect(14), Some(layout.slots[14].rect));
        assert_eq!(layout.slot_rect(19), None);
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
        let screen = [800.0, 360.0];
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
    fn inventory_rows_and_footer_have_breathing_room() {
        let registry = Registry::base();
        let items = inventory_blocks(&registry);
        for screen in [[1280.0_f32, 720.0], [960.0, 540.0]] {
            let creative = layout(screen, items.len(), 0);
            assert_eq!(creative.slot_size, INVENTORY_SLOT_SIZE);
            assert!(
                creative.footer_top - creative.grid_bottom >= INVENTORY_GRID_GAP - 0.01,
                "grid must clear the footer at {screen:?}"
            );
            assert!(creative.slots[0].rect.y - (creative.panel.y + INVENTORY_TITLE_HEIGHT) >= -0.01);
            for pair in creative
                .slots
                .windows(2)
                .filter(|pair| pair[0].rect.y == pair[1].rect.y)
            {
                assert!(pair[1].rect.x - (pair[0].rect.x + pair[0].rect.w) >= INVENTORY_GAP - 0.01);
            }

            let survival = survival_layout(screen);
            assert!(survival.footer_top - survival.grid_bottom >= INVENTORY_GRID_GAP - 0.01);
            assert!(survival.grid_bottom > survival.panel.y + INVENTORY_TITLE_HEIGHT);
            for pair in survival.slots.windows(2) {
                assert!(pair[1].x - (pair[0].x + pair[0].w) >= INVENTORY_GAP - 0.01);
                assert!((pair[0].w - INVENTORY_SLOT_SIZE).abs() < 0.01);
            }
        }
        for pair in INVENTORY_FOOTER_LINES.windows(2) {
            assert!(pair[1] - pair[0] >= ob_render::line_height(1.2) + 4.0);
        }
        let last_line_bottom = INVENTORY_FOOTER_LINES[INVENTORY_FOOTER_LINES.len() - 1] + ob_render::line_height(1.0);
        assert!(last_line_bottom <= INVENTORY_FOOTER_HEIGHT - 8.0);
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
    fn survival_layout_exposes_four_armor_slots_above_the_hotbar() {
        let screen = [1280.0, 720.0];
        let layout = survival_layout(screen);
        assert_eq!(layout.armor_slots.len(), 4);
        assert_eq!(layout.slots.len(), HOTBAR_SLOTS);
        let first_hotbar = layout.slots[0];
        for armor in &layout.armor_slots {
            assert!(armor.y + armor.h <= first_hotbar.y);
            assert!(layout.panel.contains(armor.x + 0.1, armor.y + 0.1));
            assert!(layout.panel.contains(armor.x + armor.w - 0.1, armor.y + armor.h - 0.1));
        }
        for (index, armor) in layout.armor_slots.iter().enumerate() {
            assert_eq!(
                layout.armor_hit_test(armor.center().0, armor.center().1),
                Some(
                    [
                        ArmorSlot::Helmet,
                        ArmorSlot::Chestplate,
                        ArmorSlot::Leggings,
                        ArmorSlot::Boots
                    ][index]
                )
            );
        }
        assert_eq!(survival_armor_hit_test(screen, 1.0, 1.0), None);
        assert_eq!(armor_index(ArmorSlot::Helmet), 0);
        assert_eq!(armor_index(ArmorSlot::Chestplate), 1);
        assert_eq!(armor_index(ArmorSlot::Leggings), 2);
        assert_eq!(armor_index(ArmorSlot::Boots), 3);
    }

    #[test]
    fn armor_stacks_report_durability_for_the_slot_bars() {
        let registry = Registry::base();
        let item = registry.item_id("openblock:iron_chestplate").unwrap();
        let stack = ItemStack::new(item, 1).with_durability(Some(120));
        assert_eq!(stack_durability(&registry, stack), Some((120, 240)));
        let new_stack = ItemStack::for_item(item, registry.item(item), 1);
        assert_eq!(stack_durability(&registry, new_stack), Some((240, 240)));
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
    fn item_icons_map_sticks_and_every_tool_kind_by_id() {
        use ob_content::ToolKind;
        let registry = Registry::base();
        let icon = |name: &str| {
            let item = registry.item_id(name).expect("item exists");
            item_icon_for(&registry, item).expect("icon")
        };
        assert_eq!(icon("openblock:stick"), HotbarIcon::Stick);
        assert_eq!(
            icon("openblock:wooden_pickaxe"),
            HotbarIcon::Tool {
                kind: ToolKind::Pickaxe,
                tier: 1
            }
        );
        assert_eq!(
            icon("openblock:wooden_axe"),
            HotbarIcon::Tool {
                kind: ToolKind::Axe,
                tier: 1
            }
        );
        assert_eq!(
            icon("openblock:stone_shovel"),
            HotbarIcon::Tool {
                kind: ToolKind::Shovel,
                tier: 2
            }
        );
        assert_eq!(
            icon("openblock:stone_sword"),
            HotbarIcon::Tool {
                kind: ToolKind::Sword,
                tier: 2
            }
        );
        assert_eq!(
            icon("openblock:iron_pickaxe"),
            HotbarIcon::Tool {
                kind: ToolKind::Pickaxe,
                tier: 3
            }
        );
        // New block items keep their top/side texture layers.
        let planks = registry.item_id("openblock:planks").expect("planks item");
        let planks_block = registry.id("openblock:planks").expect("planks block");
        match item_icon_for(&registry, planks).expect("planks icon") {
            HotbarIcon::Block { top, side } => {
                assert_eq!(top, u32::from(registry.block(planks_block).textures.top.0));
                assert_eq!(side, u32::from(registry.block(planks_block).textures.side.0));
            }
            other => panic!("expected a block icon, got {other:?}"),
        }
    }

    #[test]
    fn every_catalogued_item_has_an_icon_and_new_items_use_their_own_shapes() {
        let registry = Registry::base();
        assert_eq!(registry.item_len(), 52);
        for (item, item_type) in registry.items() {
            assert!(
                item_icon_for(&registry, item).is_some(),
                "{} ({item:?}) needs an icon",
                item_type.id
            );
        }
        let icon = |name: &str| {
            let item = registry.item_id(name).expect("item exists");
            item_icon_for(&registry, item).expect("icon")
        };
        assert_eq!(icon("openblock:coal"), HotbarIcon::Fuel);
        assert_eq!(icon("openblock:charcoal"), HotbarIcon::Fuel);
        assert_eq!(icon("openblock:raw_iron"), HotbarIcon::RawOre(OreKind::Iron));
        assert_eq!(icon("openblock:raw_copper"), HotbarIcon::RawOre(OreKind::Copper));
        assert_eq!(icon("openblock:iron_ingot"), HotbarIcon::Ingot(IngotKind::Iron));
        assert_eq!(icon("openblock:copper_ingot"), HotbarIcon::Ingot(IngotKind::Copper));
        assert_eq!(icon("openblock:leather"), HotbarIcon::Leather);
        assert_eq!(
            icon("openblock:leather_helmet"),
            HotbarIcon::Armor {
                slot: ArmorSlot::Helmet,
                material: ArmorMaterial::Leather
            }
        );
        assert_eq!(
            icon("openblock:iron_chestplate"),
            HotbarIcon::Armor {
                slot: ArmorSlot::Chestplate,
                material: ArmorMaterial::Iron
            }
        );
        assert_eq!(
            icon("openblock:iron_boots"),
            HotbarIcon::Armor {
                slot: ArmorSlot::Boots,
                material: ArmorMaterial::Iron
            }
        );
        assert_eq!(
            icon("openblock:raw_beef"),
            HotbarIcon::Meat {
                kind: MeatKind::Beef,
                cooked: false
            }
        );
        assert_eq!(
            icon("openblock:cooked_beef"),
            HotbarIcon::Meat {
                kind: MeatKind::Beef,
                cooked: true
            }
        );
        assert_eq!(
            icon("openblock:raw_porkchop"),
            HotbarIcon::Meat {
                kind: MeatKind::Porkchop,
                cooked: false
            }
        );
        assert_eq!(
            icon("openblock:cooked_porkchop"),
            HotbarIcon::Meat {
                kind: MeatKind::Porkchop,
                cooked: true
            }
        );
        // Fronted blocks use the furnace/chest front texture on the cube side.
        let furnace = registry.item_id("openblock:furnace").expect("furnace item");
        let furnace_block = registry.id("openblock:furnace").expect("furnace block");
        match item_icon_for(&registry, furnace).expect("furnace icon") {
            HotbarIcon::Block { top, side } => {
                let textures = registry.block(furnace_block).textures;
                assert_eq!(top, u32::from(textures.top.0));
                assert_eq!(side, u32::from(textures.front_texture().0));
                assert_ne!(
                    side,
                    u32::from(textures.side.0),
                    "furnace front must differ from its side"
                );
            }
            other => panic!("expected a block icon, got {other:?}"),
        }
        // Out-of-range ids are safe for icons, names and info.
        let unknown = ItemId(u16::MAX);
        assert_eq!(item_icon_for(&registry, unknown), None);
        assert_eq!(item_display_name(&registry, unknown), "Unknown item");
        assert_eq!(item_info_hint(&registry, unknown, None), "Unknown item");
        assert_eq!(item_tint_for(&registry, unknown), None);
    }

    #[test]
    fn procedural_catalogue_icons_stay_inside_their_box() {
        let mut cases: Vec<(&str, Vec<IconPart>)> = vec![
            ("coal", fuel_icon_parts(10.0, 20.0, 24.0)),
            ("raw iron", raw_ore_icon_parts(OreKind::Iron, 10.0, 20.0, 24.0)),
            ("raw copper", raw_ore_icon_parts(OreKind::Copper, 10.0, 20.0, 24.0)),
            ("iron ingot", ingot_icon_parts(IngotKind::Iron, 10.0, 20.0, 24.0)),
            ("copper ingot", ingot_icon_parts(IngotKind::Copper, 10.0, 20.0, 24.0)),
            ("leather", leather_icon_parts(10.0, 20.0, 24.0)),
            ("raw beef", meat_icon_parts(MeatKind::Beef, false, 10.0, 20.0, 24.0)),
            ("cooked beef", meat_icon_parts(MeatKind::Beef, true, 10.0, 20.0, 24.0)),
            (
                "raw porkchop",
                meat_icon_parts(MeatKind::Porkchop, false, 10.0, 20.0, 24.0),
            ),
            (
                "cooked porkchop",
                meat_icon_parts(MeatKind::Porkchop, true, 10.0, 20.0, 24.0),
            ),
        ];
        for slot in [
            ArmorSlot::Helmet,
            ArmorSlot::Chestplate,
            ArmorSlot::Leggings,
            ArmorSlot::Boots,
        ] {
            for material in [ArmorMaterial::Leather, ArmorMaterial::Iron] {
                cases.push(("armor", armor_icon_parts(slot, material, 10.0, 20.0, 24.0)));
            }
        }
        for (label, parts) in cases {
            assert!(!parts.is_empty(), "{label} must draw pixels");
            for part in parts {
                assert!(part.rect.w > 0.0 && part.rect.h > 0.0);
                assert!(part.rect.x >= 10.0 - 1e-4 && part.rect.y >= 20.0 - 1e-4);
                assert!(part.rect.x + part.rect.w <= 34.0 + 1e-3);
                assert!(part.rect.y + part.rect.h <= 44.0 + 1e-3);
            }
        }
        // Cooked meat is darker than the raw cuts.
        let raw = meat_icon_parts(MeatKind::Beef, false, 0.0, 0.0, 12.0);
        let cooked = meat_icon_parts(MeatKind::Beef, true, 0.0, 0.0, 12.0);
        assert_eq!(raw.len(), cooked.len());
        assert_ne!(raw[0].color, cooked[0].color);
        assert_eq!(raw[0].color, RAW_BEEF);
        assert_eq!(cooked[0].color, COOKED_BEEF);
    }

    #[test]
    fn procedural_stick_and_tool_icons_stay_inside_their_box() {
        use ob_content::ToolKind;
        for kind in [ToolKind::Pickaxe, ToolKind::Axe, ToolKind::Shovel, ToolKind::Sword] {
            for tier in [1_u8, 2, 3] {
                let parts = tool_icon_parts(kind, tier, 10.0, 20.0, 24.0);
                assert!(!parts.is_empty(), "{kind:?} tier {tier} must draw pixels");
                let expected = match tier {
                    1 => tool_head_colors(1).0,
                    2 => tool_head_colors(2).0,
                    _ => tool_head_colors(3).0,
                };
                assert!(
                    parts.iter().any(|part| part.color == expected),
                    "{kind:?} tier {tier} must use its tier head colour"
                );
                assert!(
                    parts.iter().any(|part| part.color == HANDLE_LIGHT),
                    "{kind:?} must draw a handle"
                );
                for part in &parts {
                    assert!(part.rect.w > 0.0 && part.rect.h > 0.0);
                    assert!(part.rect.x >= 10.0 - 1e-4 && part.rect.y >= 20.0 - 1e-4);
                    assert!(part.rect.x + part.rect.w <= 34.0 + 1e-3);
                    assert!(part.rect.y + part.rect.h <= 44.0 + 1e-3);
                }
            }
        }
        // Swapping the tier must change the head pixels, not the handle.
        let wood_pick = tool_icon_parts(ToolKind::Pickaxe, 1, 0.0, 0.0, 12.0);
        let stone_pick = tool_icon_parts(ToolKind::Pickaxe, 2, 0.0, 0.0, 12.0);
        assert_eq!(wood_pick.len(), stone_pick.len());
        assert_ne!(wood_pick[0].color, stone_pick[0].color);
        assert_eq!(
            wood_pick[wood_pick.len() - 1].color,
            stone_pick[stone_pick.len() - 1].color
        );

        let stick = stick_icon_parts(0.0, 0.0, 12.0);
        assert!(!stick.is_empty());
        assert!(stick.iter().all(|part| part.rect.x + part.rect.w <= 12.0 + 1e-3));
        assert!(stick.iter().all(|part| part.rect.y + part.rect.h <= 12.0 + 1e-3));
        let mut frame = UiFrame::new(1280.0, 720.0);
        for part in &stick {
            frame.rect(part.rect.x, part.rect.y, part.rect.w, part.rect.h, part.color);
        }
        let (vertices, quads) = frame.measure();
        assert_eq!(quads, stick.len());
        assert_eq!(vertices, quads * 6);
    }

    #[test]
    fn durability_bar_fraction_and_colours_are_clamped() {
        assert_eq!(durability_fraction(59, 59), 1.0);
        assert_eq!(durability_fraction(0, 59), 0.0);
        assert!((durability_fraction(30, 60) - 0.5).abs() < 1e-6);
        assert_eq!(durability_fraction(99, 59), 1.0, "malformed values clamp");
        assert_eq!(durability_fraction(5, 0), 0.0);
        assert_eq!(durability_color(1.0), [0.45, 0.78, 0.30, 1.0]);
        assert_eq!(durability_color(0.35), [0.92, 0.75, 0.25, 1.0]);
        assert_eq!(durability_color(0.1), [0.86, 0.28, 0.22, 1.0]);
        assert_eq!(durability_color(f32::NAN), [0.85, 0.28, 0.22, 1.0]);
    }

    #[test]
    fn tool_stacks_report_durability_and_the_bar_draws_inside_the_slot() {
        let registry = Registry::base();
        let stone_pick = registry.item_id("openblock:stone_pickaxe").expect("stone pickaxe item");
        let stack = ItemStack::for_item(stone_pick, registry.item(stone_pick), 1).with_durability(Some(118));
        assert_eq!(stack_durability(&registry, stack), Some((118, 131)));
        let stone = registry.item_id("openblock:stone").expect("stone item");
        assert_eq!(stack_durability(&registry, ItemStack::new(stone, 1)), None);

        let slot = UiRect::new(10.0, 20.0, 44.0, 44.0);
        let mut frame = UiFrame::new(1280.0, 720.0);
        draw_durability_bar(&mut frame, slot, 118, 131);
        let (_, quads) = frame.measure();
        assert_eq!(quads, 3, "frame, track and fill");
        let mut frame = UiFrame::new(1280.0, 720.0);
        draw_durability_bar(&mut frame, slot, 0, 131);
        let (_, quads) = frame.measure();
        assert_eq!(quads, 2, "an empty bar draws no fill cell");
    }

    #[test]
    fn undamaged_tools_do_not_draw_a_durability_bar() {
        let registry = Registry::base();
        let pick = registry.item_id("openblock:iron_pickaxe").expect("pickaxe");
        let stack = ItemStack::for_item(pick, registry.item(pick), 1);
        let slot = UiRect::new(10.0, 10.0, 48.0, 48.0);
        let mut frame = UiFrame::new(1280.0, 720.0);
        draw_stack_durability(&mut frame, slot, &registry, stack);
        let (_, quads) = frame.measure();
        assert_eq!(quads, 0, "full durability keeps the icon clean");

        let worn = stack.with_durability(Some(40));
        let mut frame = UiFrame::new(1280.0, 720.0);
        draw_stack_durability(&mut frame, slot, &registry, worn);
        let (_, quads) = frame.measure();
        assert!(quads >= 3, "damaged gear keeps the bar");
    }

    #[test]
    fn survival_control_hints_fit_the_panel_at_960x540() {
        let screen = [960.0, 540.0];
        let panel = survival_layout(screen).panel;
        let frame = UiFrame::new(screen[0], screen[1]);
        let inner = panel.w - 2.0 * SURVIVAL_PADDING;
        for hint in INVENTORY_CONTROL_HINTS {
            assert!(
                frame.text_width(1.0, hint) <= inner,
                "{hint:?} is {} px wide, panel inner width {inner}",
                frame.text_width(1.0, hint)
            );
        }
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

        let pick = registry.item_id("openblock:stone_pickaxe").expect("stone pickaxe item");
        let tool = ItemStack::for_item(pick, registry.item(pick), 1).with_durability(Some(118));
        let (name, hint) = survival_item_info(&registry, Some(tool));
        assert_eq!(name, "Stone Pickaxe");
        assert!(hint.contains("Pickaxe tier 2"), "got {hint}");
        assert!(hint.contains("Durability 118/131"), "got {hint}");

        let stick = registry.item_id("openblock:stick").expect("stick item");
        let (name, hint) = survival_item_info(&registry, Some(ItemStack::new(stick, 8)));
        assert_eq!(name, "Stick");
        assert_eq!(hint, "Crafting material");
    }

    #[test]
    fn survival_item_info_reports_armor_fuel_and_expanded_food() {
        let registry = Registry::base();
        let info = |name: &str, stack: ItemStack| {
            let item = registry.item_id(name).expect("item");
            assert_eq!(stack.item, item);
            survival_item_info(&registry, Some(stack))
        };
        // Iron helmet: exact armour footer format from the spec.
        let helmet = registry.item_id("openblock:iron_helmet").expect("iron helmet");
        let iron_stack = ItemStack::for_item(helmet, registry.item(helmet), 1);
        assert_eq!(
            info("openblock:iron_helmet", iron_stack),
            (
                "Iron Helmet".to_string(),
                "Helmet · Defence 2 · Durability 165/165".to_string()
            )
        );
        // A partially worn armor stack clamps to its remaining durability.
        let worn = iron_stack.with_durability(Some(120));
        assert_eq!(
            info("openblock:iron_helmet", worn).1,
            "Helmet · Defence 2 · Durability 120/165"
        );
        let leather = registry.item_id("openblock:leather_chestplate").expect("leather chest");
        assert_eq!(
            info(
                "openblock:leather_chestplate",
                ItemStack::for_item(leather, registry.item(leather), 1)
            )
            .1,
            "Chestplate · Defence 3 · Durability 80/80"
        );
        let beef = registry.item_id("openblock:cooked_beef").expect("cooked beef");
        assert_eq!(
            info("openblock:cooked_beef", ItemStack::new(beef, 3)).1,
            "Food +8 hunger"
        );
        let pork = registry.item_id("openblock:raw_porkchop").expect("raw porkchop");
        assert_eq!(
            info("openblock:raw_porkchop", ItemStack::new(pork, 1)).1,
            "Food +3 hunger"
        );
        let coal = registry.item_id("openblock:coal").expect("coal");
        assert_eq!(info("openblock:coal", ItemStack::new(coal, 8)).1, "Fuel (1600 ticks)");
        let charcoal = registry.item_id("openblock:charcoal").expect("charcoal");
        assert_eq!(
            info("openblock:charcoal", ItemStack::new(charcoal, 1)).1,
            "Fuel (1600 ticks)"
        );
        let raw_iron = registry.item_id("openblock:raw_iron").expect("raw iron");
        assert_eq!(
            info("openblock:raw_iron", ItemStack::new(raw_iron, 1)).1,
            "Smelt in a furnace: Iron Ingot"
        );
        let leather_item = registry.item_id("openblock:leather").expect("leather");
        assert_eq!(
            info("openblock:leather", ItemStack::new(leather_item, 1)).1,
            "Crafting material"
        );
        let iron_pick = registry.item_id("openblock:iron_pickaxe").expect("iron pickaxe");
        let pick_hint = info(
            "openblock:iron_pickaxe",
            ItemStack::for_item(iron_pick, registry.item(iron_pick), 1),
        )
        .1;
        assert!(pick_hint.contains("Durability 251/251"), "got {pick_hint}");
        assert!(pick_hint.contains("Speed 6.0"), "got {pick_hint}");
        // Unknown ids must never panic the inventory footer.
        let unknown = ItemId(u16::MAX);
        assert_eq!(
            survival_item_info(&registry, Some(ItemStack::new(unknown, 1))),
            ("Unknown item".to_string(), "Unknown item".to_string())
        );
    }

    #[test]
    fn all_catalogue_item_names_resolve() {
        let registry = Registry::base();
        let mut names = Vec::new();
        for (item, item_type) in registry.items() {
            let name = item_display_name(&registry, item);
            assert!(!name.is_empty());
            assert!(!name.contains('_'), "{name} must be title-cased");
            assert!(!name.contains(':'), "{name} must drop the namespace");
            assert_eq!(name, display_name(item_type.id));
            names.push(name);
        }
        assert_eq!(names.len(), 52);
        for expected in [
            "Iron Pickaxe",
            "Leather Chestplate",
            "Cooked Beef",
            "Cooked Porkchop",
            "Raw Copper",
            "Coal Ore",
        ] {
            assert!(names.iter().any(|name| name == expected), "missing {expected}");
        }
    }
}

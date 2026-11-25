//! Authoritative furnace and chest block entities.
//!
//! A furnace or chest placed by the host owns a small state record keyed by its
//! block position: smelting progress and three slots for a furnace, 27 slots for
//! a chest. The records are owned by [`Host`](crate::Host) and exposed through
//! [`Host::furnace_at`](crate::Host::furnace_at) and
//! [`Host::chest_at`](crate::Host::chest_at); they are ticked by the host, saved
//! in the `block_entities.txt` sidecar of the world directory and enter the host
//! state hash in position order.
//!
//! # Interim limits
//!
//! * There is no facing, lit block state or comparator output; a furnace's
//!   burning flag exists only in its record.
//! * Every stored furnace ticks every host tick, even when its chunk is not
//!   currently streamed. This keeps the rule simple and deterministic; it also
//!   means a furnace keeps cooking while the player is away.
//! * Furnaces and chests have no UI on the host. The client drives them through
//!   the slot APIs on [`Host`](crate::Host); no open-window state exists.
//! * The slot APIs do **not** validate reach, line of sight, window ownership or
//!   that the player is alive/survival. The single-player client is trusted to
//!   call them for the container it has open; a multiplayer protocol extension
//!   must add those checks before exposing the same actions to remote clients.
//! * Cook progress is not tied to a recipe id. Removing the input or filling the
//!   output pauses and later resumes the stored progress; swapping in a
//!   different smeltable input also continues from that progress.
//! * Hoppers, pistons and other automation are out of scope for this slice.

use std::collections::BTreeMap;
use std::path::Path;

use ob_content::{Registry, SMELT_TICKS_PER_ITEM};
use ob_math::BlockPos;
use ob_world::World;

use crate::{ItemStack, MAX_STACK_SIZE};

/// Number of slots in one chest.
pub const CHEST_SLOTS: usize = 27;
/// World-directory filename that stores furnace and chest records.
pub const BLOCK_ENTITIES_FILE: &str = "block_entities.txt";

/// One addressable furnace slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FurnaceSlot {
    /// The item being smelted (one item is consumed per completed smelt).
    Input,
    /// The fuel item; one item is consumed when a new burn starts.
    Fuel,
    /// The produced item. Take-only through [`Host::put_furnace_slot`]; the
    /// smelting tick merges into it but never lets it overflow.
    ///
    /// [`Host::put_furnace_slot`]: crate::Host::put_furnace_slot
    Output,
}

impl FurnaceSlot {
    /// All furnace slots in canonical order.
    pub const ALL: [FurnaceSlot; 3] = [FurnaceSlot::Input, FurnaceSlot::Fuel, FurnaceSlot::Output];

    /// Stable lower-case name used by saves and hints.
    pub const fn name(self) -> &'static str {
        match self {
            FurnaceSlot::Input => "input",
            FurnaceSlot::Fuel => "fuel",
            FurnaceSlot::Output => "output",
        }
    }

    /// Canonical array index (`Input` 0, `Fuel` 1, `Output` 2).
    pub const fn index(self) -> usize {
        match self {
            FurnaceSlot::Input => 0,
            FurnaceSlot::Fuel => 1,
            FurnaceSlot::Output => 2,
        }
    }
}

/// Authoritative state of one furnace block.
///
/// The fields are public so a future client UI can render progress and slots
/// without another host accessor, but the smelting tick is the only writer of
/// `burn_remaining`, `burn_total` and `cook_progress` in normal gameplay.
/// [`Host::furnace_at_mut`](crate::Host::furnace_at_mut) exposes a mutable
/// reference for debug tools and tests; changing progress there is authoritative
/// and will be persisted.
///
/// Invariants maintained by the tick and by loading:
/// `burn_remaining <= burn_total` and `cook_progress <= SMELT_TICKS_PER_ITEM`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FurnaceState {
    /// Stack currently being smelted.
    pub input: Option<ItemStack>,
    /// Remaining fuel stack.
    pub fuel: Option<ItemStack>,
    /// Finished smelting output.
    pub output: Option<ItemStack>,
    /// Ticks left in the current burn; `0` means unlit.
    pub burn_remaining: u16,
    /// Total ticks of the current/last burn; `0` before the first burn.
    ///
    /// It is kept after `burn_remaining` reaches zero so the UI can show the
    /// last burn's length; `burn_remaining == 0` is the lit test.
    pub burn_total: u16,
    /// Ticks already spent on the current smelt (`0..=`[`SMELT_TICKS_PER_ITEM`]).
    pub cook_progress: u16,
}

impl FurnaceState {
    /// Whether the furnace is currently consuming fuel.
    pub fn is_lit(&self) -> bool {
        self.burn_remaining > 0
    }

    /// One slot by address.
    pub fn slot(&self, slot: FurnaceSlot) -> Option<ItemStack> {
        match slot {
            FurnaceSlot::Input => self.input,
            FurnaceSlot::Fuel => self.fuel,
            FurnaceSlot::Output => self.output,
        }
    }

    /// Mutable access to one slot.
    pub fn slot_mut(&mut self, slot: FurnaceSlot) -> &mut Option<ItemStack> {
        match slot {
            FurnaceSlot::Input => &mut self.input,
            FurnaceSlot::Fuel => &mut self.fuel,
            FurnaceSlot::Output => &mut self.output,
        }
    }

    /// The non-empty contents in canonical slot order (input, fuel, output).
    pub fn contents(&self) -> Vec<ItemStack> {
        FurnaceSlot::ALL
            .iter()
            .filter_map(|slot| self.slot(*slot))
            .filter(|stack| stack.count > 0)
            .collect()
    }
}

/// Authoritative state of one chest block.
///
/// `slots` is indexed `0..`[`CHEST_SLOTS`]; an empty slot is `None`. Stack
/// counts are clamped to the content max stack by the host slot APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerState {
    /// The 27 chest slots.
    pub slots: [Option<ItemStack>; CHEST_SLOTS],
}

impl Default for ContainerState {
    fn default() -> Self {
        Self {
            slots: [None; CHEST_SLOTS],
        }
    }
}

impl ContainerState {
    /// Whether every slot is empty.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// The non-empty contents in ascending slot order.
    pub fn contents(&self) -> Vec<ItemStack> {
        self.slots
            .iter()
            .flatten()
            .copied()
            .filter(|stack| stack.count > 0)
            .collect()
    }
}

/// Advances one furnace by exactly one host tick.
///
/// Rules, in order:
///
/// 1. A smelt is *possible* while the input has a recipe and the output can
///    accept that recipe's output stack.
/// 2. When unlit and a smelt is possible, one fuel item is consumed and the burn
///    starts with the fuel's tick value (`burn_remaining == burn_total`).
///    Idle furnaces never consume fuel.
/// 3. A running burn always decrements. Cooking advances one point only while a
///    smelt is possible; otherwise progress is held (paused), never reset.
/// 4. At [`SMELT_TICKS_PER_ITEM`] one input item is consumed, the output stack is
///    merged up to the content max stack and progress resets to zero.
///
/// The function is pure in `(registry, state)` so save/reload is exact: all
/// progress lives in the state fields.
pub(crate) fn tick_furnace(registry: &Registry, state: &mut FurnaceState) {
    let recipe = state
        .input
        .filter(|stack| stack.count > 0)
        .and_then(|stack| registry.smelt_recipe_for(stack.item));
    let can_smelt = recipe.is_some_and(|(item, count)| output_accepts(registry, state.output, item, count));

    if state.burn_remaining == 0 && can_smelt {
        let burn = state
            .fuel
            .filter(|stack| stack.count > 0)
            .and_then(|stack| registry.fuel_ticks(stack.item))
            .filter(|ticks| *ticks > 0);
        if let Some(ticks) = burn {
            consume_one(&mut state.fuel);
            state.burn_remaining = ticks;
            state.burn_total = ticks;
        }
    }

    if state.burn_remaining == 0 {
        return;
    }
    state.burn_remaining -= 1;

    if !can_smelt {
        state.cook_progress = state.cook_progress.min(SMELT_TICKS_PER_ITEM);
        return;
    }

    state.cook_progress = state.cook_progress.saturating_add(1).min(SMELT_TICKS_PER_ITEM);
    if state.cook_progress < SMELT_TICKS_PER_ITEM {
        return;
    }
    let Some((output, output_count)) = recipe else {
        return;
    };
    if !output_accepts(registry, state.output, output, output_count) {
        return;
    }
    add_output(registry, &mut state.output, output, output_count);
    consume_one(&mut state.input);
    state.cook_progress = 0;
}

/// Whether `output` has room for one recipe output stack.
fn output_accepts(registry: &Registry, output: Option<ItemStack>, item: ob_content::ItemId, count: u8) -> bool {
    if count == 0 {
        return false;
    }
    let Some(existing) = output.filter(|stack| stack.count > 0) else {
        return true;
    };
    if existing.item != item || existing.durability != default_durability(registry, item) {
        return false;
    }
    let max_stack = u16::from(registry.item(item).max_stack.clamp(1, MAX_STACK_SIZE));
    u16::from(existing.count) + u16::from(count) <= max_stack
}

/// Merges one recipe output into the output slot; the caller checked capacity.
fn add_output(registry: &Registry, output: &mut Option<ItemStack>, item: ob_content::ItemId, count: u8) {
    let fresh = ItemStack::for_item(item, registry.item(item), count);
    match output {
        Some(existing) if existing.item == fresh.item && existing.durability == fresh.durability => {
            let max_stack = registry.item(item).max_stack.clamp(1, MAX_STACK_SIZE);
            existing.count = existing.count.saturating_add(count).min(max_stack);
        }
        _ => *output = Some(fresh),
    }
}

/// Full durability of an item that declares one (tools and armor).
fn default_durability(registry: &Registry, item: ob_content::ItemId) -> Option<u16> {
    let item_type = registry.item(item);
    item_type
        .tool
        .map(|spec| spec.durability)
        .or_else(|| item_type.armor.map(|spec| spec.durability))
}

/// Removes one item from a slot, clearing the slot when it reaches zero.
pub(crate) fn consume_one(slot: &mut Option<ItemStack>) {
    if let Some(stack) = slot {
        if stack.count > 1 {
            stack.count -= 1;
        } else {
            *slot = None;
        }
    }
}

/// Serializes both block-entity maps to deterministic `block_entities.txt`
/// lines: every furnace first, then every chest, each group in ascending
/// `(x, y, z)` position order.
///
/// Formats:
///
/// ```text
/// furnace <x> <y> <z> <burn_remaining> <burn_total> <cook_progress> <input> <fuel> <output>
/// chest <x> <y> <z> [<slot>:<stack> ...]
/// ```
///
/// A stack token is `-` when empty, otherwise `item,count` or
/// `item,count,durability`. Chest entries list only non-empty slots in ascending
/// slot order. Empty furnaces and chests are written too: their block entity must
/// still exist after a reload.
pub(crate) fn block_entities_state(
    registry: &Registry,
    furnaces: &BTreeMap<BlockPos, FurnaceState>,
    chests: &BTreeMap<BlockPos, ContainerState>,
) -> String {
    let mut text = String::new();
    for (pos, state) in furnaces {
        let input = encode_slot(registry, &state.input, "furnace input");
        let fuel = encode_slot(registry, &state.fuel, "furnace fuel");
        let output = encode_slot(registry, &state.output, "furnace output");
        text.push_str(&format!(
            "furnace {} {} {} {} {} {} {input} {fuel} {output}\n",
            pos.x, pos.y, pos.z, state.burn_remaining, state.burn_total, state.cook_progress
        ));
    }
    for (pos, state) in chests {
        text.push_str(&format!("chest {} {} {}", pos.x, pos.y, pos.z));
        for (slot, stack) in state.slots.iter().enumerate() {
            let Some(stack) = stack.filter(|stack| stack.count > 0) else {
                continue;
            };
            match encode_stack(registry, &stack) {
                Some(token) => text.push_str(&format!(" {slot}:{token}")),
                None => log::warn!(
                    "chest at ({}, {}, {}) slot {slot}: invalid stack skipped",
                    pos.x,
                    pos.y,
                    pos.z
                ),
            }
        }
        text.push('\n');
    }
    text
}

fn encode_slot(registry: &Registry, slot: &Option<ItemStack>, label: &str) -> String {
    let Some(stack) = slot.filter(|stack| stack.count > 0) else {
        return "-".to_string();
    };
    encode_stack(registry, &stack).unwrap_or_else(|| {
        log::warn!("{label}: invalid stack skipped while saving");
        "-".to_string()
    })
}

fn encode_stack(registry: &Registry, stack: &ItemStack) -> Option<String> {
    if stack.count == 0 || stack.item.0 as usize >= registry.item_len() {
        return None;
    }
    let name = registry.item(stack.item).id;
    Some(match stack.durability {
        Some(remaining) => format!("{name},{},{remaining}", stack.count),
        None => format!("{name},{}", stack.count),
    })
}

/// Loads `block_entities.txt`.
///
/// A missing file (a save from before this slice) yields empty maps. Malformed
/// lines are skipped with a warning; a malformed furnace slot skips that furnace
/// line, and a malformed chest slot token skips only that slot. Unknown item
/// names and out-of-range values are handled the same way as the other loaders.
/// Records are then pruned against the world ([`prune_block_entities`]): a
/// loaded chunk whose block is not the expected furnace/chest removes the
/// record with a warning. Records in unloaded chunks are kept because the block
/// cannot be inspected yet.
pub(crate) fn load_block_entities(
    world_dir: &Path,
    registry: &Registry,
    world: &World,
) -> (BTreeMap<BlockPos, FurnaceState>, BTreeMap<BlockPos, ContainerState>) {
    let path = world_dir.join(BLOCK_ENTITIES_FILE);
    let text = match crate::read_sidecar(&path) {
        Ok(text) => text,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "could not read {}: {err}; starting with no block entities",
                    path.display()
                );
            }
            return (BTreeMap::new(), BTreeMap::new());
        }
    };

    let mut furnaces = BTreeMap::new();
    let mut chests = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields[0] {
            "furnace" => {
                if let Some((pos, state)) = parse_furnace(&fields, registry, line_number) {
                    if let std::collections::btree_map::Entry::Vacant(entry) = furnaces.entry(pos) {
                        entry.insert(state);
                    } else {
                        log::warn!(
                            "block_entities.txt:{line_number}: duplicate furnace at ({}, {}, {}), dropping it",
                            pos.x,
                            pos.y,
                            pos.z
                        );
                    }
                }
            }
            "chest" => {
                if let Some((pos, state)) = parse_chest(&fields, registry, line_number) {
                    if let std::collections::btree_map::Entry::Vacant(entry) = chests.entry(pos) {
                        entry.insert(state);
                    } else {
                        log::warn!(
                            "block_entities.txt:{line_number}: duplicate chest at ({}, {}, {}), dropping it",
                            pos.x,
                            pos.y,
                            pos.z
                        );
                    }
                }
            }
            other => log::warn!("block_entities.txt:{line_number}: unknown record kind '{other}'"),
        }
    }

    prune_block_entities(world, registry, &mut furnaces, &mut chests);
    (furnaces, chests)
}

fn parse_furnace(fields: &[&str], registry: &Registry, line_number: usize) -> Option<(BlockPos, FurnaceState)> {
    if fields.len() != 10 {
        log::warn!("block_entities.txt:{line_number}: malformed furnace line");
        return None;
    }
    let pos = parse_position(fields, line_number)?;
    let burn_remaining = parse_u16(fields[4], "burn_remaining", line_number)?;
    let burn_total = parse_u16(fields[5], "burn_total", line_number)?.max(burn_remaining);
    let cook_progress = parse_u16(fields[6], "cook_progress", line_number)?.min(SMELT_TICKS_PER_ITEM);

    let input = parse_optional_stack(fields[7], registry, line_number)?;
    let fuel = parse_optional_stack(fields[8], registry, line_number)?;
    let output = parse_optional_stack(fields[9], registry, line_number)?;
    Some((
        pos,
        FurnaceState {
            input,
            fuel,
            output,
            burn_remaining,
            burn_total,
            cook_progress,
        },
    ))
}

fn parse_chest(fields: &[&str], registry: &Registry, line_number: usize) -> Option<(BlockPos, ContainerState)> {
    if fields.len() < 4 {
        log::warn!("block_entities.txt:{line_number}: malformed chest line");
        return None;
    }
    let pos = parse_position(fields, line_number)?;
    let mut state = ContainerState::default();
    for token in &fields[4..] {
        let Some((slot, stack_token)) = token.split_once(':') else {
            log::warn!("block_entities.txt:{line_number}: malformed chest slot '{token}'");
            continue;
        };
        let Ok(slot) = slot.parse::<usize>() else {
            log::warn!("block_entities.txt:{line_number}: invalid chest slot '{slot}'");
            continue;
        };
        if slot >= CHEST_SLOTS {
            log::warn!("block_entities.txt:{line_number}: chest slot {slot} is out of range");
            continue;
        }
        let Some(stack) = parse_stack(stack_token, registry, line_number) else {
            continue;
        };
        if state.slots[slot].is_some() {
            log::warn!("block_entities.txt:{line_number}: duplicate chest slot {slot}");
            continue;
        }
        state.slots[slot] = Some(stack);
    }
    Some((pos, state))
}

fn parse_position(fields: &[&str], line_number: usize) -> Option<BlockPos> {
    let x = fields[1].parse::<i32>().ok();
    let y = fields[2].parse::<i32>().ok();
    let z = fields[3].parse::<i32>().ok();
    let (Some(x), Some(y), Some(z)) = (x, y, z) else {
        log::warn!("block_entities.txt:{line_number}: invalid block position");
        return None;
    };
    let pos = BlockPos::new(x, y, z);
    if !pos.in_world() {
        log::warn!("block_entities.txt:{line_number}: block position outside the world");
        return None;
    }
    Some(pos)
}

fn parse_u16(text: &str, label: &str, line_number: usize) -> Option<u16> {
    text.parse::<u16>().ok().or_else(|| {
        log::warn!("block_entities.txt:{line_number}: invalid {label} '{text}'");
        None
    })
}

/// Parses one `-`/`item,count[,durability]` furnace token.
///
/// The outer `Option` distinguishes a malformed token (`None`, which skips the
/// whole furnace line) from a valid empty slot (`Some(None)`).
fn parse_optional_stack(token: &str, registry: &Registry, line_number: usize) -> Option<Option<ItemStack>> {
    if token == "-" {
        return Some(None);
    }
    parse_stack(token, registry, line_number).map(Some)
}

fn parse_stack(token: &str, registry: &Registry, line_number: usize) -> Option<ItemStack> {
    let mut parts = token.split(',');
    let name = parts.next().unwrap_or_default();
    let Some(count) = parts.next() else {
        log::warn!("block_entities.txt:{line_number}: malformed stack '{token}'");
        return None;
    };
    let durability = parts.next();
    if parts.next().is_some() {
        log::warn!("block_entities.txt:{line_number}: malformed stack '{token}'");
        return None;
    }
    let Some(item) = registry.item_id(name) else {
        log::warn!("block_entities.txt:{line_number}: unknown item '{name}', dropping it");
        return None;
    };
    let Ok(count) = count.parse::<u32>() else {
        log::warn!("block_entities.txt:{line_number}: invalid stack count '{count}'");
        return None;
    };
    let item_type = registry.item(item);
    let max_stack = item_type.max_stack.clamp(1, MAX_STACK_SIZE);
    let count = count.clamp(1, u32::from(max_stack)) as u8;
    let mut stack = ItemStack::for_item(item, item_type, count);
    if let Some(text) = durability {
        let Ok(value) = text.parse::<u32>() else {
            log::warn!("block_entities.txt:{line_number}: invalid durability '{text}'");
            return None;
        };
        match default_durability(registry, item) {
            Some(max) => stack.durability = Some(value.clamp(1, u32::from(max)) as u16),
            None => {
                log::warn!("block_entities.txt:{line_number}: item '{name}' has no durability, ignoring it");
            }
        }
    }
    Some(stack)
}

/// Removes furnace and chest records whose block no longer exists.
///
/// Only loaded chunks can be inspected: a record in an unloaded chunk is kept.
/// This runs on load and again before every save, so a block removed through a
/// direct [`World`] edit is cleaned up at the next save (with a warning) instead
/// of lingering in the sidecar and the state hash.
pub(crate) fn prune_block_entities(
    world: &World,
    registry: &Registry,
    furnaces: &mut BTreeMap<BlockPos, FurnaceState>,
    chests: &mut BTreeMap<BlockPos, ContainerState>,
) {
    let furnace = registry.id_or_air("openblock:furnace");
    let chest = registry.id_or_air("openblock:chest");
    furnaces.retain(|pos, _| {
        if !pos.in_world() {
            log::warn!(
                "removing furnace record outside the world at ({}, {}, {})",
                pos.x,
                pos.y,
                pos.z
            );
            return false;
        }
        if world.chunk(pos.chunk()).is_none() {
            return true;
        }
        let actual = world.get_block(*pos);
        if actual != furnace {
            log::warn!(
                "removing stale furnace record at ({}, {}, {}): block is {}",
                pos.x,
                pos.y,
                pos.z,
                registry.block(actual).id
            );
            return false;
        }
        true
    });
    chests.retain(|pos, _| {
        if !pos.in_world() {
            log::warn!(
                "removing chest record outside the world at ({}, {}, {})",
                pos.x,
                pos.y,
                pos.z
            );
            return false;
        }
        if world.chunk(pos.chunk()).is_none() {
            return true;
        }
        let actual = world.get_block(*pos);
        if actual != chest {
            log::warn!(
                "removing stale chest record at ({}, {}, {}): block is {}",
                pos.x,
                pos.y,
                pos.z,
                registry.block(actual).id
            );
            return false;
        }
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_content::Registry;
    use std::collections::BTreeMap;

    fn stack(registry: &Registry, name: &str, count: u8) -> ItemStack {
        let item = registry.item_id(name).expect("item exists");
        ItemStack::for_item(item, registry.item(item), count)
    }

    #[test]
    fn furnace_tick_consumes_fuel_and_smelts() {
        let registry = Registry::base();
        let mut state = FurnaceState {
            input: Some(stack(&registry, "openblock:raw_iron", 2)),
            fuel: Some(stack(&registry, "openblock:coal", 1)),
            ..FurnaceState::default()
        };
        for _ in 0..SMELT_TICKS_PER_ITEM {
            tick_furnace(&registry, &mut state);
        }
        assert_eq!(state.output, Some(stack(&registry, "openblock:iron_ingot", 1)));
        assert_eq!(state.input.unwrap().count, 1);
        assert_eq!(state.fuel, None);
        assert_eq!(state.cook_progress, 0);
        assert_eq!(state.burn_total, 1_600);
        assert_eq!(state.burn_remaining, 1_400);
        assert!(state.is_lit());
    }

    #[test]
    fn furnace_tick_idles_without_input_and_pauses_when_output_is_full() {
        let registry = Registry::base();
        let mut idle = FurnaceState {
            fuel: Some(stack(&registry, "openblock:coal", 1)),
            ..FurnaceState::default()
        };
        for _ in 0..100 {
            tick_furnace(&registry, &mut idle);
        }
        assert_eq!(idle.fuel.unwrap().count, 1, "idle furnaces never burn fuel");
        assert_eq!(idle.burn_remaining, 0);

        let mut blocked = FurnaceState {
            input: Some(stack(&registry, "openblock:raw_iron", 3)),
            fuel: Some(stack(&registry, "openblock:coal", 1)),
            output: Some(stack(&registry, "openblock:iron_ingot", 64)),
            ..FurnaceState::default()
        };
        let iron = registry.item_id("openblock:iron_ingot").unwrap();
        for _ in 0..50 {
            tick_furnace(&registry, &mut blocked);
        }
        assert_eq!(blocked.cook_progress, 0, "a full output blocks cooking");
        assert_eq!(blocked.fuel.unwrap().count, 1, "no fuel is wasted while blocked");
        assert_eq!(blocked.output.unwrap().count, 64);
        assert_eq!(blocked.input.unwrap().count, 3);
        assert_eq!(blocked.output.unwrap().item, iron);
    }

    #[test]
    fn block_entity_save_format_round_trips_through_parser() {
        let registry = Registry::base();
        let mut furnaces = BTreeMap::new();
        let mut chests = BTreeMap::new();
        let pos = BlockPos::new(1, 70, -2);
        furnaces.insert(
            pos,
            FurnaceState {
                input: Some(stack(&registry, "openblock:raw_iron", 2)),
                fuel: Some(stack(&registry, "openblock:coal", 1)),
                output: None,
                burn_remaining: 500,
                burn_total: 1_600,
                cook_progress: 42,
            },
        );
        let mut chest = ContainerState::default();
        chest.slots[0] = Some(stack(&registry, "openblock:dirt", 10));
        chests.insert(BlockPos::new(3, 64, 4), chest);
        let text = block_entities_state(&registry, &furnaces, &chests);
        assert!(text.starts_with("furnace 1 70 -2 500 1600 42 openblock:raw_iron,2 openblock:coal,1 -\n"));
        assert!(text.contains("chest 3 64 4 0:openblock:dirt,10\n"));

        let furnace_fields: Vec<&str> = text.lines().next().unwrap().split_whitespace().collect();
        let (parsed_pos, parsed) = parse_furnace(&furnace_fields, &registry, 1).expect("furnace parses");
        assert_eq!(parsed_pos, pos);
        assert_eq!(parsed, furnaces[&pos]);

        let chest_fields: Vec<&str> = text.lines().nth(1).unwrap().split_whitespace().collect();
        let (parsed_pos, parsed) = parse_chest(&chest_fields, &registry, 2).expect("chest parses");
        assert_eq!(parsed_pos, BlockPos::new(3, 64, 4));
        assert_eq!(parsed, chests[&BlockPos::new(3, 64, 4)]);
        assert_eq!(FurnaceSlot::Input.index(), 0);
        assert_eq!(FurnaceSlot::Output.name(), "output");
        assert_eq!(
            furnaces[&pos].contents(),
            vec![
                stack(&registry, "openblock:raw_iron", 2),
                stack(&registry, "openblock:coal", 1)
            ]
        );
    }
}

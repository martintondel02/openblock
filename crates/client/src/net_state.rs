//! Pure server-authoritative state for the protocol v3 client.
//!
//! This module is deliberately free of sockets, threads, winit and the
//! renderer. It turns protocol `ServerMessage` payloads into the state the
//! windowed client draws and the intent helpers that turn UI clicks into
//! `ClientMessage` transactions. Everything here is a reducer over immutable
//! bytes plus wall-clock timestamps, so the survival, container and entity
//! behaviour is unit-testable without a server.
//!
//! Ownership split:
//!
//! * [`SurvivalState`] mirrors vitals, the 36 player inventory slots, the four
//!   armor slots and the death/respawn handshake for the local player.
//! * [`ReplicatedEntities`] mirrors mobs and dropped items with opaque ids and
//!   short position interpolation.
//! * [`RemoteContainers`] mirrors the one open furnace/chest window and turns
//!   cursor clicks into `inventory_move` intents with the latest revision.
//! * [`RemoteTransfer`] is the neutral form of one `inventory_move`.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Instant;

use ob_content::{ItemId, MobKind, Registry};
use ob_math::WorldVec;
use ob_protocol::{
    ContainerKind as WireContainerKind, DespawnReason, EQUIPMENT_SLOTS, EntityKind, EntitySnapshot, EntityStateEntry,
    FurnaceProgress, HOTBAR_SLOTS, INVENTORY_SLOTS, ItemStackDto, MAIN_SLOTS, SLOT_WINDOW_START,
};
use ob_render::EntityInstance;
use ob_server::{CHEST_SLOTS, ContainerState, FurnaceState, ItemStack, MAX_STACK_SIZE};

use crate::containers::{ContainerKind, ContainerSnapshot, SlotId};
use crate::mobs::{MobVisual, mob_instances_from_visuals};
use crate::ui_state::HOTBAR_SLOTS as UI_HOTBAR_SLOTS;

/// Duration of one entity interpolation segment (matches the 10 Hz snapshots).
pub const ENTITY_INTERPOLATION_SECONDS: f32 = 0.1;
/// Movement in metres above which an item/mob is rendered as walking.
const ENTITY_MOVE_EPSILON: f64 = 0.01;
/// Maximum number of damage events retained for the HUD.
pub const MAX_DAMAGE_EVENTS: usize = 16;
/// Maximum number of damage events drained per frame.
pub const MAX_DAMAGE_DRAIN: usize = 8;
/// Maximum entities applied from one `entity_snapshot`/`entity_state` batch.
///
/// The protocol frame cap already bounds the decoded array; this tighter cap
/// bounds the reducer, interpolation and UI work a hostile server can trigger
/// with one message.
pub const MAX_ENTITY_BATCH: usize = 4096;
/// Maximum live entities cached from snapshots and spawns.
///
/// Reaching the cap refuses new ids but still updates existing ones, so a
/// stream of spawns cannot grow the client's entity map without bound.
pub const MAX_REPLICATED_ENTITIES: usize = 8192;

/// One damage event from the server, queued for HUD feedback.
#[derive(Debug, Clone, PartialEq)]
pub struct DamageEvent {
    /// Damage removed after armor.
    pub amount: f32,
    /// Short cause (`fall`, `starvation`, `mob`, ...).
    pub cause: String,
}

/// The local network player's survival state.
///
/// All fields are authoritative server state. The client may predict block
/// visuals, but never health, hunger, inventory counts, durability or the
/// selected slot.
#[derive(Debug, Clone)]
pub struct SurvivalState {
    /// Health in half-hearts, `0..=20`.
    pub health: f32,
    /// Hunger points, `0..=20`.
    pub hunger: f32,
    /// Effective armor defense points.
    pub armor_defense: u8,
    /// 36 player inventory stacks: hotbar `0..9`, main `9..36`.
    pub inventory: [Option<ItemStack>; INVENTORY_SLOTS],
    /// Four equipment (armor) stacks.
    pub equipment: [Option<ItemStack>; EQUIPMENT_SLOTS],
    /// Selected hotbar slot.
    pub selected: usize,
    /// Whether the server has declared the player dead.
    pub dead: bool,
    /// Death cause for the death screen.
    pub death_cause: Option<String>,
    /// Whether a `respawn` intent has been sent and the reply is pending.
    pub respawn_requested: bool,
    /// Pending damage events for flash/shake/sound.
    damage_events: VecDeque<DamageEvent>,
    /// Whether a vitals message has arrived (survival HUD readiness).
    pub vitals_received: bool,
    /// Whether an inventory sync has arrived.
    pub inventory_received: bool,
}

impl Default for SurvivalState {
    fn default() -> Self {
        Self {
            health: 20.0,
            hunger: 20.0,
            armor_defense: 0,
            inventory: [None; INVENTORY_SLOTS],
            equipment: [None; EQUIPMENT_SLOTS],
            selected: 0,
            dead: false,
            death_cause: None,
            respawn_requested: false,
            damage_events: VecDeque::new(),
            vitals_received: false,
            inventory_received: false,
        }
    }
}

impl SurvivalState {
    /// Resets every field to a fresh, disconnected session.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Applies `player_vitals`.
    pub fn apply_vitals(&mut self, health: f32, hunger: f32, armor_defense: u8) {
        if health.is_finite() {
            self.health = health.clamp(0.0, 20.0);
        }
        if hunger.is_finite() {
            self.hunger = hunger.clamp(0.0, 20.0);
        }
        self.armor_defense = armor_defense.min(20);
        self.vitals_received = true;
    }

    /// Applies a full `inventory_sync`.
    pub fn apply_inventory(
        &mut self,
        registry: &Registry,
        hotbar: &[Option<ItemStackDto>; HOTBAR_SLOTS],
        main: &[Option<ItemStackDto>; MAIN_SLOTS],
        selected: u8,
        equipment: &[Option<ItemStackDto>; EQUIPMENT_SLOTS],
    ) {
        for (slot, dto) in self.inventory.iter_mut().zip(hotbar) {
            *slot = dto.as_ref().and_then(|dto| stack_from_dto(registry, dto));
        }
        for (slot, dto) in self.inventory[HOTBAR_SLOTS..].iter_mut().zip(main) {
            *slot = dto.as_ref().and_then(|dto| stack_from_dto(registry, dto));
        }
        for (slot, dto) in self.equipment.iter_mut().zip(equipment) {
            *slot = dto.as_ref().and_then(|dto| stack_from_dto(registry, dto));
        }
        self.selected = usize::from(selected).min(HOTBAR_SLOTS - 1);
        self.inventory_received = true;
    }

    /// Applies `selected_slot`.
    pub fn apply_selected(&mut self, slot: u8) {
        self.selected = usize::from(slot).min(HOTBAR_SLOTS - 1);
    }

    /// Records one damage event and marks the player as hurt.
    pub fn apply_damage(&mut self, amount: f32, cause: &str) {
        if !amount.is_finite() || amount <= 0.0 {
            return;
        }
        if self.damage_events.len() >= MAX_DAMAGE_EVENTS {
            self.damage_events.pop_front();
        }
        self.damage_events.push_back(DamageEvent {
            amount,
            cause: cause.to_string(),
        });
    }

    /// Applies `death`.
    pub fn apply_death(&mut self, cause: &str) {
        self.dead = true;
        self.health = 0.0;
        self.death_cause = Some(cause.to_string());
        self.respawn_requested = false;
    }

    /// Applies `respawn`.
    pub fn apply_respawn(&mut self, health: f32, hunger: f32) {
        self.dead = false;
        self.death_cause = None;
        self.respawn_requested = false;
        self.health = if health.is_finite() {
            health.clamp(0.0, 20.0)
        } else {
            20.0
        };
        self.hunger = if hunger.is_finite() {
            hunger.clamp(0.0, 20.0)
        } else {
            20.0
        };
    }

    /// Removes and returns up to [`MAX_DAMAGE_DRAIN`] queued damage events.
    pub fn drain_damage_events(&mut self) -> Vec<DamageEvent> {
        let take = self.damage_events.len().min(MAX_DAMAGE_DRAIN);
        self.damage_events.drain(..take).collect()
    }

    /// Number of queued damage events (for diagnostics and tests).
    pub fn pending_damage_events(&self) -> usize {
        self.damage_events.len()
    }

    /// Selected hotbar stack.
    pub fn selected_stack(&self) -> Option<ItemStack> {
        self.inventory.get(self.selected).copied().flatten()
    }

    /// The first nine inventory slots (the hotbar).
    pub fn hotbar(&self) -> &[Option<ItemStack>] {
        &self.inventory[..HOTBAR_SLOTS]
    }
}

/// Converts one wire stack into the client's server-side stack type.
///
/// Unknown item names and zero counts become `None`; counts clamp to the
/// content maximum exactly like the server's write path. Durability is kept
/// as sent (display only).
pub fn stack_from_dto(registry: &Registry, dto: &ItemStackDto) -> Option<ItemStack> {
    if dto.count == 0 {
        return None;
    }
    let item = registry.item_id(&dto.item)?;
    let max_stack = registry.item(item).max_stack.clamp(1, MAX_STACK_SIZE);
    Some(ItemStack {
        item,
        count: dto.count.min(max_stack),
        durability: dto.durability,
    })
}

/// Stable wire name for one client stack, used by intent tests and logs.
pub fn dto_from_stack(registry: &Registry, stack: ItemStack) -> Option<ItemStackDto> {
    if stack.item.0 as usize >= registry.item_len() {
        return None;
    }
    Some(ItemStackDto {
        item: registry.item(stack.item).id.to_string(),
        count: stack.count,
        durability: stack.durability,
    })
}

/// Live entity kind mirrored from the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplicatedKind {
    /// A mob species.
    Mob(MobKind),
    /// A dropped item stack.
    Item(ItemStack),
}

#[derive(Debug, Clone)]
struct CachedEntity {
    kind: ReplicatedKind,
    health: f32,
    hurt: bool,
    from_pos: WorldVec,
    to_pos: WorldVec,
    from_yaw: f32,
    to_yaw: f32,
    updated: Instant,
    moving: bool,
}

impl CachedEntity {
    fn new(kind: ReplicatedKind, pos: WorldVec, yaw: f32, health: f32, hurt: bool, now: Instant) -> Self {
        Self {
            kind,
            health,
            hurt,
            from_pos: pos,
            to_pos: pos,
            from_yaw: yaw,
            to_yaw: yaw,
            updated: now,
            moving: false,
        }
    }

    fn update(&mut self, pos: WorldVec, yaw: f32, health: f32, hurt: bool, now: Instant) {
        self.moving = self.to_pos.distance(pos) > ENTITY_MOVE_EPSILON;
        self.from_pos = self.to_pos;
        self.from_yaw = self.to_yaw;
        self.to_pos = pos;
        self.to_yaw = yaw;
        self.health = if health.is_finite() { health } else { self.health };
        self.hurt = hurt;
        self.updated = now;
    }

    fn interpolated(&self, now: Instant) -> (WorldVec, f32) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f32();
        let t = (elapsed / ENTITY_INTERPOLATION_SECONDS).clamp(0.0, 1.0);
        let mut delta = (self.to_yaw - self.from_yaw) % std::f32::consts::TAU;
        if delta > std::f32::consts::PI {
            delta -= std::f32::consts::TAU;
        } else if delta < -std::f32::consts::PI {
            delta += std::f32::consts::TAU;
        }
        (self.from_pos.lerp(self.to_pos, f64::from(t)), self.from_yaw + delta * t)
    }
}

/// One mob ray target for [`ob_sim::raycast_mob`].
struct NetMobTarget {
    id: u64,
    kind: MobKind,
    pos: WorldVec,
}

impl ob_sim::MobTarget for NetMobTarget {
    fn mob_id(&self) -> u64 {
        self.id
    }

    fn mob_kind(&self) -> MobKind {
        self.kind
    }

    fn mob_pos(&self) -> WorldVec {
        self.pos
    }
}

/// Mobs and dropped items replicated by the server, keyed by opaque wire id.
///
/// The registry never interprets an id as a host save id: item entities use
/// the high bit on the server, but the client only ever compares ids for
/// equality. Snapshot/spawn/state/despawn reducers keep the map consistent, and
/// interpolation lags the latest 10 Hz state by at most
/// [`ENTITY_INTERPOLATION_SECONDS`].
#[derive(Debug, Clone, Default)]
pub struct ReplicatedEntities {
    entries: HashMap<u64, CachedEntity>,
    last_despawn: Option<(DespawnReason, usize)>,
}

impl ReplicatedEntities {
    /// Clears every entity (disconnect/reconnect).
    pub fn reset(&mut self) {
        self.entries.clear();
        self.last_despawn = None;
    }

    /// Replaces the registry with a join `entity_snapshot`.
    ///
    /// At most [`MAX_ENTITY_BATCH`] entries are applied; any extra entries in
    /// an oversized message are dropped without touching the map.
    pub fn apply_snapshot(&mut self, registry: &Registry, entities: Vec<EntitySnapshot>, now: Instant) -> usize {
        self.entries.clear();
        for entity in entities.into_iter().take(MAX_ENTITY_BATCH) {
            self.insert_snapshot(registry, entity, now);
        }
        self.entries.len()
    }

    /// Inserts or replaces one `entity_spawn` payload.
    pub fn apply_spawn(&mut self, registry: &Registry, entity: EntitySnapshot, now: Instant) -> bool {
        self.insert_snapshot(registry, entity, now)
    }

    fn insert_snapshot(&mut self, registry: &Registry, entity: EntitySnapshot, now: Instant) -> bool {
        let Some(kind) = kind_from_wire(registry, &entity.kind) else {
            return false;
        };
        if !valid_entity_pose(entity.pos, entity.yaw, entity.health) {
            return false;
        }
        if !self.entries.contains_key(&entity.id) && self.entries.len() >= MAX_REPLICATED_ENTITIES {
            return false;
        }
        let pos = WorldVec::new(entity.pos[0], entity.pos[1], entity.pos[2]);
        self.entries.insert(
            entity.id,
            CachedEntity::new(kind, pos, entity.yaw, entity.health, entity.hurt, now),
        );
        true
    }

    /// Applies one batched `entity_state`; unknown ids and entries beyond
    /// [`MAX_ENTITY_BATCH`] are ignored.
    pub fn apply_state(&mut self, registry: &Registry, entries: &[EntityStateEntry], now: Instant) -> usize {
        let mut updated = 0;
        for entry in entries.iter().take(MAX_ENTITY_BATCH) {
            let Some(cached) = self.entries.get_mut(&entry.id) else {
                continue;
            };
            if !valid_entity_pose(entry.pos, entry.yaw, entry.health) {
                continue;
            }
            let pos = WorldVec::new(entry.pos[0], entry.pos[1], entry.pos[2]);
            cached.update(pos, entry.yaw, entry.health, entry.hurt, now);
            if let ReplicatedKind::Item(stack) = &mut cached.kind
                && let Some(dto) = &entry.stack
                && let Some(replacement) = stack_from_dto(registry, dto)
            {
                *stack = replacement;
            }
            updated += 1;
        }
        updated
    }

    /// Removes despawned entities and remembers the reason.
    ///
    /// The batch is bounded by the live map capacity; extras can only be
    /// no-ops, so they are ignored to keep a hostile payload's work bounded.
    pub fn apply_despawn(&mut self, ids: &[u64], reason: DespawnReason) -> usize {
        let mut removed = 0;
        for id in ids.iter().take(MAX_REPLICATED_ENTITIES) {
            if self.entries.remove(id).is_some() {
                removed += 1;
            }
            if self.entries.is_empty() {
                break;
            }
        }
        if removed > 0 {
            self.last_despawn = Some((reason, removed));
        }
        removed
    }

    /// Number of tracked entities.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entities are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Live mob count.
    pub fn mob_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entity| matches!(entity.kind, ReplicatedKind::Mob(_)))
            .count()
    }

    /// Live dropped-item count.
    pub fn item_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entity| matches!(entity.kind, ReplicatedKind::Item(_)))
            .count()
    }

    /// Passive mob count (for the F3 overlay).
    pub fn passive_mob_count(&self, registry: &Registry) -> usize {
        self.entries
            .values()
            .filter(|entity| match entity.kind {
                ReplicatedKind::Mob(kind) => registry.mob(kind).passive(),
                ReplicatedKind::Item(_) => false,
            })
            .count()
    }

    /// Hostile mob count (for the F3 overlay).
    pub fn hostile_mob_count(&self, registry: &Registry) -> usize {
        self.mob_count().saturating_sub(self.passive_mob_count(registry))
    }

    /// Most recent despawn reason and removed count.
    pub fn last_despawn(&self) -> Option<(DespawnReason, usize)> {
        self.last_despawn
    }

    /// One entity snapshot by id, for tests and diagnostics.
    pub fn kind(&self, id: u64) -> Option<&ReplicatedKind> {
        self.entries.get(&id).map(|entity| &entity.kind)
    }

    /// Health of one mob, if tracked.
    pub fn health(&self, id: u64) -> Option<f32> {
        self.entries.get(&id).map(|entity| entity.health)
    }

    /// Interpolated position of one tracked entity (diagnostics/tests).
    pub fn position(&self, id: u64, now: Instant) -> Option<WorldVec> {
        self.entries.get(&id).map(|entity| entity.interpolated(now).0)
    }

    /// Whether one mob is in its hurt state.
    pub fn hurt(&self, id: u64) -> Option<bool> {
        self.entries.get(&id).map(|entity| entity.hurt)
    }

    /// A sorted list of every tracked id (deterministic draw order).
    pub fn ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.entries.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Interpolated mob poses ordered by id.
    pub fn mob_visuals(&self, now: Instant, elapsed: f32) -> Vec<MobVisual> {
        let mut ids = self.ids();
        ids.retain(|id| {
            matches!(
                self.entries.get(id).map(|entity| &entity.kind),
                Some(ReplicatedKind::Mob(_))
            )
        });
        ids.into_iter()
            .filter_map(|id| {
                let entity = self.entries.get(&id)?;
                let ReplicatedKind::Mob(kind) = &entity.kind else {
                    return None;
                };
                let (pos, yaw) = entity.interpolated(now);
                Some(MobVisual {
                    kind: *kind,
                    pos,
                    yaw,
                    hurt: entity.hurt,
                    walk_phase: elapsed * 8.0 + id as f32 * 0.9,
                    moving: entity.moving,
                })
            })
            .collect()
    }

    /// Mob cubes for the renderer.
    pub fn mob_instances(&self, registry: &Registry, now: Instant, elapsed: f32) -> Vec<EntityInstance> {
        mob_instances_from_visuals(registry, &self.mob_visuals(now, elapsed))
    }

    /// Item cubes for the renderer, using the shared item tint/layer mapping.
    pub fn item_instances(&self, registry: &Registry, now: Instant, elapsed: f32) -> Vec<EntityInstance> {
        let mut instances = Vec::with_capacity(self.item_count());
        for id in self.ids() {
            let Some(entity) = self.entries.get(&id) else {
                continue;
            };
            let ReplicatedKind::Item(stack) = entity.kind else {
                continue;
            };
            let (pos, _yaw) = entity.interpolated(now);
            instances.push(crate::item_entity_instance(id, pos, stack, registry, elapsed));
        }
        instances
    }

    /// Ray-casts the crosshair against every replicated mob.
    pub fn raycast_mob(
        &self,
        registry: &Registry,
        origin: WorldVec,
        direction: WorldVec,
        max_distance: f64,
        now: Instant,
    ) -> Option<u64> {
        let mut targets = Vec::with_capacity(self.mob_count());
        for id in self.ids() {
            let Some(entity) = self.entries.get(&id) else {
                continue;
            };
            let ReplicatedKind::Mob(kind) = entity.kind else {
                continue;
            };
            let (pos, _yaw) = entity.interpolated(now);
            targets.push(NetMobTarget { id, kind, pos });
        }
        ob_sim::raycast_mob(registry, &targets, origin, direction, max_distance)
    }
}

fn valid_entity_pose(pos: [f64; 3], yaw: f32, health: f32) -> bool {
    pos.iter().all(|value| value.is_finite()) && yaw.is_finite() && health.is_finite()
}

fn kind_from_wire(registry: &Registry, kind: &EntityKind) -> Option<ReplicatedKind> {
    match kind {
        EntityKind::Mob { mob } => registry.mob_id(mob).map(ReplicatedKind::Mob),
        EntityKind::Item { stack } => stack_from_dto(registry, stack).map(ReplicatedKind::Item),
    }
}

/// One cursor-preview stack and the authoritative slot it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorStack {
    /// Preview stack shown attached to the cursor.
    pub stack: ItemStack,
    /// Slot address (`0..9` hotbar or `100..` window) it was picked from.
    pub source: u16,
}

/// The open server window mirrored by the client.
#[derive(Debug, Clone)]
pub struct RemoteWindow {
    /// Window id from `container_open`.
    pub id: u32,
    /// Furnace or chest.
    pub kind: WireContainerKind,
    /// Block position of the container.
    pub pos: ob_math::BlockPos,
    /// Latest content revision.
    pub revision: u64,
    /// Window slots (`3` furnace, `27` chest).
    pub slots: Vec<Option<ItemStack>>,
    /// Latest furnace progress, if a furnace.
    pub furnace: Option<FurnaceProgress>,
}

impl RemoteWindow {
    /// UI container kind.
    pub fn ui_kind(&self) -> ContainerKind {
        match self.kind {
            WireContainerKind::Furnace => ContainerKind::Furnace,
            WireContainerKind::Chest => ContainerKind::Chest,
        }
    }

    /// Furnace state for the existing furnace drawing code.
    pub fn furnace_state(&self) -> Option<FurnaceState> {
        if self.kind != WireContainerKind::Furnace {
            return None;
        }
        let progress = self.furnace.unwrap_or(FurnaceProgress {
            burn_remaining: 0,
            burn_total: 0,
            cook_progress: 0,
        });
        Some(FurnaceState {
            input: self.slots.first().copied().flatten(),
            fuel: self.slots.get(1).copied().flatten(),
            output: self.slots.get(2).copied().flatten(),
            burn_remaining: progress.burn_remaining,
            burn_total: progress.burn_total,
            cook_progress: progress.cook_progress,
        })
    }

    /// Chest state for the existing chest drawing code.
    pub fn chest_state(&self) -> Option<ContainerState> {
        if self.kind != WireContainerKind::Chest {
            return None;
        }
        Some(ContainerState {
            slots: std::array::from_fn(|index| self.slots.get(index).copied().flatten()),
        })
    }

    /// Stack in one window slot.
    pub fn stack(&self, index: usize) -> Option<ItemStack> {
        self.slots.get(index).copied().flatten()
    }
}

/// One `inventory_move` intent produced by a container click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteTransfer {
    /// Source slot address.
    pub from: u16,
    /// Destination slot address.
    pub to: u16,
    /// Items to move (at least one).
    pub count: u8,
    /// Latest window revision when a window slot is involved.
    pub revision: Option<u64>,
}

impl RemoteTransfer {
    /// Converts the intent into the protocol message.
    pub fn to_client_message(self) -> ob_protocol::ClientMessage {
        ob_protocol::ClientMessage::InventoryMove {
            from: self.from,
            to: self.to,
            count: self.count,
            revision: self.revision,
        }
    }
}

/// Result of one remote container click.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteClickOutcome {
    /// Transfer to queue, if the click requested a server transaction.
    pub transfer: Option<RemoteTransfer>,
    /// User-facing refusal/hint, if the click cannot be represented remotely.
    pub message: Option<String>,
}

impl RemoteClickOutcome {
    fn none() -> Self {
        Self {
            transfer: None,
            message: None,
        }
    }

    fn refused(message: impl Into<String>) -> Self {
        Self {
            transfer: None,
            message: Some(message.into()),
        }
    }
}

/// The one open remote furnace/chest window plus the local cursor preview.
///
/// The wire protocol has no server-side cursor slot, so the cursor is a pure
/// client preview: picking a stack does not mutate the server. Placing it
/// emits exactly one `inventory_move` from the pick source to the clicked slot,
/// and the authoritative `container_update`/`inventory_sync` frames replace the
/// preview shortly afterwards.
#[derive(Debug, Clone, Default)]
pub struct RemoteContainers {
    window: Option<RemoteWindow>,
    cursor: Option<CursorStack>,
}

impl RemoteContainers {
    /// Clears the window and cursor (disconnect/reconnect).
    pub fn reset(&mut self) {
        self.window = None;
        self.cursor = None;
    }

    /// The open window, if any.
    pub fn window(&self) -> Option<&RemoteWindow> {
        self.window.as_ref()
    }

    /// Open window id.
    pub fn window_id(&self) -> Option<u32> {
        self.window.as_ref().map(|window| window.id)
    }

    /// Latest revision of the open window.
    pub fn revision(&self) -> Option<u64> {
        self.window.as_ref().map(|window| window.revision)
    }

    /// The local cursor preview, if any.
    pub fn cursor(&self) -> Option<CursorStack> {
        self.cursor
    }

    /// Replaces the cursor (used to roll back a failed send).
    pub fn set_cursor(&mut self, cursor: Option<CursorStack>) {
        self.cursor = cursor;
    }

    /// Applies `container_open`, replacing any previous window and cursor.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &mut self,
        id: u32,
        kind: WireContainerKind,
        pos: ob_math::BlockPos,
        revision: u64,
        slots: Vec<Option<ItemStackDto>>,
        furnace: Option<FurnaceProgress>,
        registry: &Registry,
    ) {
        let expected = match kind {
            WireContainerKind::Furnace => ob_protocol::FURNACE_WINDOW_SLOTS,
            WireContainerKind::Chest => ob_protocol::CHEST_WINDOW_SLOTS,
        };
        // Only the expected window width is ever stored or converted; extras
        // in a hostile payload are ignored instead of allocating a wider vec.
        let mut normalised: Vec<Option<ItemStack>> = slots
            .iter()
            .take(expected)
            .map(|dto| dto.as_ref().and_then(|dto| stack_from_dto(registry, dto)))
            .collect();
        normalised.resize(expected, None);
        self.window = Some(RemoteWindow {
            id,
            kind,
            pos,
            revision,
            slots: normalised,
            furnace: if kind == WireContainerKind::Furnace {
                furnace
            } else {
                None
            },
        });
        self.cursor = None;
    }

    /// Applies `container_update`; stale revisions and unknown windows are ignored.
    ///
    /// Furnace progress updates reuse the current revision, so equal revisions
    /// are accepted. A window-sourced cursor preview that no longer fits the
    /// new authoritative slot is dropped and the return value reports it;
    /// hotbar-sourced cursors are validated by [`Self::apply_hotbar`] when the
    /// following `inventory_sync` arrives.
    pub fn update(
        &mut self,
        registry: &Registry,
        id: u32,
        revision: u64,
        slots: Vec<Option<ItemStackDto>>,
        furnace: Option<FurnaceProgress>,
    ) -> RemoteUpdate {
        let Some(window) = self.window.as_mut() else {
            return RemoteUpdate::IGNORED;
        };
        if window.id != id || revision < window.revision {
            return RemoteUpdate::IGNORED;
        }
        let expected = match window.kind {
            WireContainerKind::Furnace => ob_protocol::FURNACE_WINDOW_SLOTS,
            WireContainerKind::Chest => ob_protocol::CHEST_WINDOW_SLOTS,
        };
        let mut normalised: Vec<Option<ItemStack>> = slots
            .iter()
            .take(expected)
            .map(|dto| dto.as_ref().and_then(|dto| stack_from_dto(registry, dto)))
            .collect();
        normalised.resize(expected, None);
        let slots_changed = window.slots != normalised;
        window.slots = normalised;
        window.revision = revision;
        if window.kind == WireContainerKind::Furnace {
            window.furnace = furnace;
        }
        // A window update only confirms window slots. A cursor picked from the
        // hotbar may be mid-swap until the following inventory sync arrives.
        let cursor_dropped = !self.cursor_window_valid();
        if cursor_dropped {
            self.cursor = None;
        }
        RemoteUpdate {
            accepted: true,
            slots_changed,
            cursor_dropped,
        }
    }

    /// Applies `container_close` for the matching window; other ids are ignored.
    pub fn close(&mut self, id: u32) -> bool {
        if self.window.as_ref().is_some_and(|window| window.id == id) {
            self.window = None;
            self.cursor = None;
            true
        } else {
            false
        }
    }

    /// Closes whatever window is open (death, local close, disconnect).
    pub fn close_current(&mut self) -> bool {
        let had = self.window.is_some();
        self.window = None;
        self.cursor = None;
        had
    }

    /// Validates a hotbar-sourced cursor against a new `inventory_sync`.
    ///
    /// Window-sourced cursors are left alone: the container update that follows
    /// a window transaction owns their validation.
    pub fn apply_hotbar(&mut self, hotbar: &[Option<ItemStack>]) -> bool {
        if self.cursor.is_some() && !self.cursor_hotbar_valid(hotbar) {
            self.cursor = None;
            return true;
        }
        false
    }

    fn cursor_window_valid(&self) -> bool {
        let Some(cursor) = self.cursor else {
            return true;
        };
        if cursor.source < SLOT_WINDOW_START {
            return true;
        }
        let Some(window) = self.window.as_ref() else {
            return false;
        };
        let Some(source) = address_to_slot(window, cursor.source) else {
            return false;
        };
        window_source_stack(window, source)
            .is_some_and(|stack| stack.item == cursor.stack.item && stack.count >= cursor.stack.count)
    }

    fn cursor_hotbar_valid(&self, hotbar: &[Option<ItemStack>]) -> bool {
        let Some(cursor) = self.cursor else {
            return true;
        };
        if cursor.source >= SLOT_WINDOW_START {
            return true;
        }
        hotbar.get(usize::from(cursor.source)).is_some_and(|stack| {
            stack.is_some_and(|stack| stack.item == cursor.stack.item && stack.count >= cursor.stack.count)
        })
    }

    /// The authoritative snapshot without cursor shadowing.
    pub fn authoritative(&self, hotbar: &[Option<ItemStack>]) -> Option<ContainerSnapshot> {
        self.window
            .as_ref()
            .map(|window| authoritative_snapshot(window, hotbar))
    }

    /// The snapshot the UI draws, with the cursor preview subtracted.
    pub fn snapshot(&self, hotbar: &[Option<ItemStack>]) -> Option<ContainerSnapshot> {
        let window = self.window.as_ref()?;
        let mut snapshot = authoritative_snapshot(window, hotbar);
        if let Some(cursor) = self.cursor {
            subtract_from_snapshot(&mut snapshot, window, cursor);
        }
        Some(snapshot)
    }

    /// Plans one click and updates the local cursor preview.
    ///
    /// The returned transfer is already expressed as a protocol
    /// `inventory_move`; the caller only has to queue it.
    pub fn click(
        &mut self,
        registry: &Registry,
        hotbar: &[Option<ItemStack>],
        slot: SlotId,
        button: crate::containers::ClickButton,
    ) -> RemoteClickOutcome {
        use crate::containers::ClickButton;
        let Some(window) = self.window.as_ref() else {
            return RemoteClickOutcome::refused("No container is open");
        };
        if !slot_matches_kind(window.kind, slot) {
            return RemoteClickOutcome::refused("That slot belongs to another container");
        }
        let Some(target_address) = slot_to_address(slot) else {
            return RemoteClickOutcome::refused("That slot cannot be addressed");
        };
        let authoritative = authoritative_snapshot(window, hotbar);

        let Some(cursor) = self.cursor else {
            let Some(mut stack) = authoritative.stack(slot) else {
                return RemoteClickOutcome::none();
            };
            if stack.count == 0 {
                return RemoteClickOutcome::none();
            }
            let half = button == ClickButton::Right;
            let take = if slot == SlotId::FurnaceOutput || !half {
                stack.count
            } else {
                stack.count.div_ceil(2)
            };
            stack.count = take;
            self.cursor = Some(CursorStack {
                stack,
                source: target_address,
            });
            return RemoteClickOutcome::none();
        };

        if target_address == cursor.source {
            // Clicking the source puts the preview back.
            self.cursor = None;
            return RemoteClickOutcome::none();
        }
        if slot == SlotId::FurnaceOutput {
            return RemoteClickOutcome::refused("Place the held stack before taking the output");
        }
        let target = authoritative.stack(slot);
        let one = button == ClickButton::Right;
        let wanted = if one { 1 } else { cursor.stack.count };
        let mut remaining = cursor.stack.count;

        let (moved, cursor_after) = match target {
            None => {
                remaining = remaining.saturating_sub(wanted);
                (wanted, Some(remaining))
            }
            Some(target) if stacks_merge(target, cursor.stack) => {
                let max = max_stack(registry, target.item);
                let room = max.saturating_sub(target.count);
                if room == 0 {
                    return RemoteClickOutcome::refused("That stack is full");
                }
                let moved = wanted.min(room);
                remaining = remaining.saturating_sub(moved);
                (moved, Some(remaining))
            }
            Some(target) => {
                if one {
                    return RemoteClickOutcome::refused("Right-click only merges matching items; left-click swaps");
                }
                if cursor.source == 102 {
                    return RemoteClickOutcome::refused("Move the output into an empty or matching slot");
                }
                let authoritative_source = authoritative
                    .stack(address_to_slot(window, cursor.source).unwrap_or(SlotId::Hotbar(0)))
                    .map_or(0, |stack| stack.count);
                if cursor.stack.count != authoritative_source {
                    return RemoteClickOutcome::refused("Click with the whole stack to swap into an occupied slot");
                }
                // The server swaps the whole source stack with the target.
                self.cursor = Some(CursorStack {
                    stack: target,
                    source: cursor.source,
                });
                let transfer = RemoteTransfer {
                    from: cursor.source,
                    to: target_address,
                    count: cursor.stack.count,
                    revision: revision_for(window, cursor.source, target_address),
                };
                return RemoteClickOutcome {
                    transfer: Some(transfer),
                    message: None,
                };
            }
        };
        if moved == 0 {
            return RemoteClickOutcome::refused("Nothing can be moved there");
        }
        if cursor_after.is_some_and(|count| count == 0) {
            self.cursor = None;
        } else if let Some(count) = cursor_after {
            self.cursor = Some(CursorStack {
                stack: ItemStack { count, ..cursor.stack },
                source: cursor.source,
            });
        }
        RemoteClickOutcome {
            transfer: Some(RemoteTransfer {
                from: cursor.source,
                to: target_address,
                count: moved,
                revision: revision_for(window, cursor.source, target_address),
            }),
            message: None,
        }
    }
}

/// Outcome of applying a `container_update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteUpdate {
    /// Whether the update belonged to the open window and was not stale.
    pub accepted: bool,
    /// Whether slot contents changed (furnace progress alone does not count).
    pub slots_changed: bool,
    /// Whether the local cursor preview was dropped because it no longer fits.
    pub cursor_dropped: bool,
}

impl RemoteUpdate {
    /// An update that was not applied (no window or stale revision).
    pub const IGNORED: Self = Self {
        accepted: false,
        slots_changed: false,
        cursor_dropped: false,
    };
}

impl Default for RemoteUpdate {
    fn default() -> Self {
        Self::IGNORED
    }
}

fn authoritative_snapshot(window: &RemoteWindow, hotbar: &[Option<ItemStack>]) -> ContainerSnapshot {
    let mut inventory = [None; HOTBAR_SLOTS];
    for (destination, source) in inventory.iter_mut().zip(hotbar.iter()) {
        *destination = *source;
    }
    ContainerSnapshot {
        furnace: window.furnace_state(),
        chest: window.chest_state().unwrap_or_default().slots,
        inventory,
    }
}

fn window_source_stack(window: &RemoteWindow, slot: SlotId) -> Option<ItemStack> {
    match slot {
        SlotId::Hotbar(_) => None,
        SlotId::FurnaceInput => window.stack(0),
        SlotId::FurnaceFuel => window.stack(1),
        SlotId::FurnaceOutput => window.stack(2),
        SlotId::Chest(index) => window.stack(index),
    }
}

fn subtract_from_snapshot(snapshot: &mut ContainerSnapshot, window: &RemoteWindow, cursor: CursorStack) {
    let Some(slot) = address_to_slot(window, cursor.source) else {
        return;
    };
    let subtract = |stack: Option<ItemStack>| -> Option<ItemStack> {
        let stack = stack?;
        if stack.item != cursor.stack.item || stack.count < cursor.stack.count {
            return Some(stack);
        }
        let left = stack.count - cursor.stack.count;
        (left > 0).then_some(ItemStack { count: left, ..stack })
    };
    match slot {
        SlotId::Hotbar(index) => {
            if let Some(slot) = snapshot.inventory.get_mut(index) {
                *slot = subtract(*slot);
            }
        }
        SlotId::FurnaceInput => {
            if let Some(state) = snapshot.furnace.as_mut() {
                state.input = subtract(state.input);
            }
        }
        SlotId::FurnaceFuel => {
            if let Some(state) = snapshot.furnace.as_mut() {
                state.fuel = subtract(state.fuel);
            }
        }
        SlotId::FurnaceOutput => {
            if let Some(state) = snapshot.furnace.as_mut() {
                state.output = subtract(state.output);
            }
        }
        SlotId::Chest(index) => {
            if let Some(slot) = snapshot.chest.get_mut(index) {
                *slot = subtract(*slot);
            }
        }
    }
}

fn slot_matches_kind(kind: WireContainerKind, slot: SlotId) -> bool {
    match kind {
        WireContainerKind::Furnace => slot.furnace_slot().is_some() || matches!(slot, SlotId::Hotbar(_)),
        WireContainerKind::Chest => matches!(slot, SlotId::Chest(_) | SlotId::Hotbar(_)),
    }
}

/// Wire address of one clickable slot.
pub fn slot_to_address(slot: SlotId) -> Option<u16> {
    match slot {
        SlotId::Hotbar(index) => u16::try_from(index).ok().filter(|index| *index < HOTBAR_SLOTS as u16),
        SlotId::FurnaceInput => Some(SLOT_WINDOW_START),
        SlotId::FurnaceFuel => Some(SLOT_WINDOW_START + 1),
        SlotId::FurnaceOutput => Some(SLOT_WINDOW_START + 2),
        SlotId::Chest(index) => u16::try_from(index)
            .ok()
            .filter(|index| *index < ob_protocol::CHEST_WINDOW_SLOTS as u16)
            .map(|index| SLOT_WINDOW_START + index),
    }
}

/// Slot addressed by one wire address within an open window.
pub fn address_to_slot(window: &RemoteWindow, address: u16) -> Option<SlotId> {
    match address {
        0..=8 => Some(SlotId::Hotbar(usize::from(address))),
        9..=12 => None,
        value if value >= SLOT_WINDOW_START => {
            let index = usize::from(value - SLOT_WINDOW_START);
            match window.kind {
                WireContainerKind::Furnace => match index {
                    0 => Some(SlotId::FurnaceInput),
                    1 => Some(SlotId::FurnaceFuel),
                    2 => Some(SlotId::FurnaceOutput),
                    _ => None,
                },
                WireContainerKind::Chest => (index < CHEST_SLOTS).then_some(SlotId::Chest(index)),
            }
        }
        _ => None,
    }
}

/// Equipment wire address for one armor slot index (helmet first).
pub fn equipment_address(index: usize) -> Option<u16> {
    (index < EQUIPMENT_SLOTS).then(|| ob_protocol::EQUIPMENT_SLOT_START + index as u16)
}

fn revision_for(window: &RemoteWindow, from: u16, to: u16) -> Option<u64> {
    (from >= SLOT_WINDOW_START || to >= SLOT_WINDOW_START).then_some(window.revision)
}

fn max_stack(registry: &Registry, item: ItemId) -> u8 {
    if item.0 as usize >= registry.item_len() {
        return 1;
    }
    registry.item(item).max_stack.clamp(1, MAX_STACK_SIZE)
}

fn stacks_merge(a: ItemStack, b: ItemStack) -> bool {
    a.item == b.item && a.durability == b.durability
}

/// Maps a server cause code to a death-screen sentence.
pub fn friendly_death_cause(cause: &str) -> String {
    match cause.trim() {
        "fall" => "You fell from a high place".to_string(),
        "starvation" => "You starved to death".to_string(),
        "mob" => "You were slain by a mob".to_string(),
        "admin" => "An administrator ended your life".to_string(),
        "void" => "You fell out of the world".to_string(),
        "" => "Cause: lost all health".to_string(),
        other => format!("Cause: {other}"),
    }
}

/// Maps an `action_rejected` pair to a short UI hint.
pub fn friendly_action_rejection(action: &str, reason: &str) -> String {
    let text = match reason.trim() {
        "out_of_reach" => "Too far away",
        "dead" => "You are dead",
        "rejected" => "The server rejected that action",
        "invalid_block" => "That block cannot be placed",
        "invalid_position" => "That position is not loaded",
        "no_block" => "There is no block there",
        "unbreakable" => "That block cannot be broken",
        "occupied" => "That space is occupied",
        "player_overlap" => "You are standing there",
        "no_support" => "That block needs support",
        "not_held" => "Select the matching item first",
        "invalid_slot" => "Invalid slot",
        "invalid_count" => "Invalid item count",
        "same_slot" => "Source and destination are the same",
        "empty_source" => "That slot is empty",
        "destination_full" => "That stack is full",
        "output_take_only" => "The furnace output is take-only",
        "destination_occupied" => "The destination slot is occupied",
        "invalid_equipment_move" => "Armor cannot move there",
        "not_armor" => "That item is not armor",
        "wrong_armor_slot" => "Wrong armor slot",
        "equipment_single_item" => "Armor moves one piece at a time",
        "invalid_equipment_swap" => "That armor cannot be swapped there",
        "no_window" => "No container is open",
        "stale_revision" => "The container changed; try again",
        "container_missing" => "That container is gone",
        "not_container" => "That block has no container",
        "not_open" => "No container is open",
        "unloaded" => "That chunk is not loaded",
        "inventory_full" => "Your inventory is full",
        "not_edible_or_full" => "Not edible or hunger is full",
        "not_equippable" => "That item cannot be equipped",
        "not_dead" => "You are not dead",
        "no_target" => "Nothing is in reach",
        "cooldown" => "Slow down",
        "rate_limited" => "Slow down",
        "invalid_stack" => "Invalid item stack",
        "not_connected" => "Not connected",
        "" => "Action rejected",
        other => other,
    };
    let action = action.trim();
    if action.is_empty() || matches!(text, "You are dead" | "Not connected") {
        text.to_string()
    } else {
        format!("{text} ({action})")
    }
}

/// Empty hotbar helper for tests and defaults.
pub fn empty_hotbar() -> [Option<ItemStack>; HOTBAR_SLOTS] {
    [None; HOTBAR_SLOTS]
}

/// Ensures the UI hotbar count stays in sync with the protocol count.
const _: () = assert!(UI_HOTBAR_SLOTS == HOTBAR_SLOTS);

#[cfg(test)]
mod tests {
    use super::*;
    use ob_content::Registry;
    use ob_math::BlockPos;
    use std::time::Duration;

    fn registry() -> Registry {
        Registry::base()
    }

    fn stack(registry: &Registry, name: &str, count: u8, durability: Option<u16>) -> ItemStack {
        ItemStack {
            item: registry.item_id(name).expect("item"),
            count,
            durability,
        }
    }

    fn dto(registry: &Registry, name: &str, count: u8, durability: Option<u16>) -> ItemStackDto {
        dto_from_stack(registry, stack(registry, name, count, durability)).expect("dto")
    }

    fn furnace_open(registry: &Registry) -> RemoteContainers {
        let mut containers = RemoteContainers::default();
        containers.open(
            7,
            WireContainerKind::Furnace,
            BlockPos::new(1, 2, 3),
            4,
            vec![
                Some(dto(registry, "openblock:raw_iron", 2, None)),
                Some(dto(registry, "openblock:coal", 3, None)),
                None,
            ],
            Some(FurnaceProgress {
                burn_remaining: 100,
                burn_total: 200,
                cook_progress: 50,
            }),
            registry,
        );
        containers
    }

    #[test]
    fn vitals_inventory_selected_and_damage_reduce() {
        let registry = registry();
        let mut state = SurvivalState::default();
        assert!(!state.vitals_received);
        state.apply_vitals(17.5, 12.0, 7);
        assert_eq!(state.health, 17.5);
        assert_eq!(state.hunger, 12.0);
        assert_eq!(state.armor_defense, 7);
        assert!(state.vitals_received);
        state.apply_vitals(f32::NAN, -4.0, 99);
        assert_eq!(state.health, 17.5);
        assert_eq!(state.hunger, 0.0);
        assert_eq!(state.armor_defense, 20);

        let hotbar: [Option<ItemStackDto>; HOTBAR_SLOTS] =
            std::array::from_fn(|index| (index == 2).then(|| dto(&registry, "openblock:stone", 12, None)));
        let equipment: [Option<ItemStackDto>; EQUIPMENT_SLOTS] =
            std::array::from_fn(|index| (index == 0).then(|| dto(&registry, "openblock:iron_helmet", 1, Some(55))));
        let main: [Option<ItemStackDto>; MAIN_SLOTS] =
            std::array::from_fn(|index| (index == 0).then(|| dto(&registry, "openblock:dirt", 5, None)));
        state.apply_inventory(&registry, &hotbar, &main, 9, &equipment);
        assert!(state.inventory_received);
        assert_eq!(state.selected, 8, "out-of-range selected slots clamp");
        assert_eq!(state.inventory[2].map(|stack| stack.count), Some(12));
        assert_eq!(
            state.inventory[9].map(|stack| stack.count),
            Some(5),
            "main slots map to addresses 9..36"
        );
        assert!(state.equipment[0].is_some());
        state.apply_selected(2);
        assert_eq!(state.selected_stack().map(|stack| stack.count), Some(12));

        state.apply_damage(3.5, "fall");
        state.apply_damage(-1.0, "ignored");
        assert_eq!(state.pending_damage_events(), 1);
        let events = state.drain_damage_events();
        assert_eq!(events[0].amount, 3.5);
        assert_eq!(events[0].cause, "fall");
        assert!(state.drain_damage_events().is_empty());
    }

    #[test]
    fn equipment_wire_addresses_match_protocol_v3() {
        assert_eq!(equipment_address(0), Some(36));
        assert_eq!(equipment_address(1), Some(37));
        assert_eq!(equipment_address(2), Some(38));
        assert_eq!(equipment_address(3), Some(39));
        assert_eq!(equipment_address(4), None);
        assert_eq!(slot_to_address(SlotId::Hotbar(8)), Some(8));
        assert_eq!(slot_to_address(SlotId::Hotbar(9)), None);
        assert_eq!(slot_to_address(SlotId::FurnaceInput), Some(SLOT_WINDOW_START));
    }

    #[test]
    fn hud_icons_follow_synced_vitals() {
        let mut state = SurvivalState::default();
        state.apply_vitals(17.5, 3.0, 7);
        let hearts = crate::ui_state::heart_states(state.health);
        assert_eq!(hearts[0], crate::ui_state::HeartState::Full);
        assert_eq!(hearts[8], crate::ui_state::HeartState::Half);
        assert_eq!(hearts[9], crate::ui_state::HeartState::Empty);
        let hunger = crate::ui_state::hunger_states(state.hunger);
        assert_eq!(hunger[0], crate::ui_state::HungerState::Full);
        assert_eq!(hunger[1], crate::ui_state::HungerState::Half);
        assert_eq!(hunger[2], crate::ui_state::HungerState::Empty);
        assert_eq!(state.armor_defense, 7);
    }

    #[test]
    fn reset_returns_the_session_to_a_clean_baseline() {
        let registry = registry();
        let now = Instant::now();
        let mut state = SurvivalState::default();
        state.apply_vitals(4.0, 3.0, 5);
        state.apply_damage(2.0, "mob");
        state.apply_death("mob");
        state.reset();
        assert_eq!(state.health, 20.0);
        assert_eq!(state.hunger, 20.0);
        assert!(state.inventory.iter().all(Option::is_none));
        assert!(!state.dead);
        assert!(state.death_cause.is_none());
        assert_eq!(state.pending_damage_events(), 0);

        let mut entities = ReplicatedEntities::default();
        entities.apply_snapshot(
            &registry,
            vec![snapshot(1, mob_kind("openblock:cow"), [0.0; 3], 10.0)],
            now,
        );
        entities.reset();
        assert!(entities.is_empty());
        assert_eq!(entities.last_despawn(), None);

        let mut containers = furnace_open(&registry);
        containers.reset();
        assert!(containers.window().is_none());
        assert!(containers.cursor().is_none());
    }

    #[test]
    fn unknown_items_do_not_poison_inventory_state() {
        let registry = registry();
        let mut state = SurvivalState::default();
        let hotbar: [Option<ItemStackDto>; HOTBAR_SLOTS] = std::array::from_fn(|index| match index {
            0 => Some(ItemStackDto {
                item: "openblock:not_an_item".to_string(),
                count: 3,
                durability: None,
            }),
            1 => Some(ItemStackDto {
                item: "openblock:stone".to_string(),
                count: 0,
                durability: None,
            }),
            _ => None,
        });
        state.apply_inventory(
            &registry,
            &hotbar,
            &std::array::from_fn(|_| None),
            0,
            &std::array::from_fn(|_| None),
        );
        assert!(state.inventory.iter().all(Option::is_none));
        assert!(stack_from_dto(&registry, &dto(&registry, "openblock:stone", 1, None)).is_some());
    }

    #[test]
    fn death_and_respawn_flow_resets_the_death_screen() {
        let mut state = SurvivalState::default();
        state.apply_vitals(4.0, 20.0, 0);
        state.apply_death("mob");
        assert!(state.dead);
        assert_eq!(state.health, 0.0);
        assert_eq!(state.death_cause.as_deref(), Some("mob"));
        state.respawn_requested = true;
        state.apply_respawn(20.0, 18.0);
        assert!(!state.dead);
        assert_eq!(state.health, 20.0);
        assert_eq!(state.hunger, 18.0);
        assert!(!state.respawn_requested);
        assert!(state.death_cause.is_none());
        assert_eq!(friendly_death_cause("fall"), "You fell from a high place");
        assert_eq!(friendly_death_cause("mystery"), "Cause: mystery");
    }

    fn snapshot(id: u64, kind: EntityKind, pos: [f64; 3], health: f32) -> EntitySnapshot {
        EntitySnapshot {
            id,
            kind,
            pos,
            yaw: 0.0,
            health,
            hurt: false,
        }
    }

    fn mob_kind(name: &str) -> EntityKind {
        EntityKind::Mob { mob: name.to_string() }
    }

    #[test]
    fn entity_reducers_accept_opaque_ids_and_interpolate() {
        let registry = registry();
        let now = Instant::now();
        let mut entities = ReplicatedEntities::default();
        let ids = [1u64, u64::MAX, 42];
        let snapshots = vec![
            snapshot(ids[0], mob_kind("openblock:cow"), [0.0, 64.0, 0.0], 10.0),
            snapshot(
                ids[1],
                EntityKind::Item {
                    stack: dto(&registry, "openblock:stone", 4, None),
                },
                [4.0, 65.0, 4.0],
                0.0,
            ),
            snapshot(ids[2], mob_kind("openblock:zombie"), [2.0, 64.0, 2.0], 20.0),
        ];
        assert_eq!(entities.apply_snapshot(&registry, snapshots, now), 3);
        assert_eq!(entities.mob_count(), 2);
        assert_eq!(entities.item_count(), 1);
        assert!(entities.kind(u64::MAX).is_some(), "bit-63 ids are opaque");
        assert_eq!(entities.health(ids[0]), Some(10.0));

        let state_entries = vec![
            EntityStateEntry {
                id: ids[0],
                pos: [2.0, 64.0, 0.0],
                yaw: 0.5,
                health: 7.5,
                hurt: true,
                stack: None,
            },
            EntityStateEntry {
                id: 999,
                pos: [0.0, 0.0, 0.0],
                yaw: 0.0,
                health: 1.0,
                hurt: false,
                stack: None,
            },
        ];
        assert_eq!(
            entities.apply_state(&registry, &state_entries, now),
            1,
            "unknown ids ignored"
        );
        assert_eq!(entities.health(ids[0]), Some(7.5));
        assert_eq!(entities.hurt(ids[0]), Some(true));
        let visuals = entities.mob_visuals(now + Duration::from_millis(50), 1.0);
        assert_eq!(visuals.len(), 2);
        let cow = visuals.iter().find(|visual| visual.kind == MobKind::Cow).expect("cow");
        assert!((cow.pos.x - 1.0).abs() < 1e-6, "halfway between snapshots");
        assert!((cow.pos.y - 64.0).abs() < 1e-6);
        assert!(cow.hurt);
        let arrived = entities.mob_visuals(now + Duration::from_secs(1), 1.0);
        let cow = arrived.iter().find(|visual| visual.kind == MobKind::Cow).expect("cow");
        assert!((cow.pos.x - 2.0).abs() < 1e-6);
        assert!(
            !entities
                .mob_instances(&registry, now + Duration::from_secs(1), 1.0)
                .is_empty()
        );
        assert_eq!(
            entities
                .item_instances(&registry, now + Duration::from_secs(1), 1.0)
                .len(),
            1
        );
    }

    #[test]
    fn entity_state_updates_item_counts_and_despawn_cleans_up() {
        let registry = registry();
        let now = Instant::now();
        let mut entities = ReplicatedEntities::default();
        entities.apply_snapshot(
            &registry,
            vec![snapshot(
                u64::MAX,
                EntityKind::Item {
                    stack: dto(&registry, "openblock:stone", 4, None),
                },
                [0.0, 64.0, 0.0],
                0.0,
            )],
            now,
        );
        entities.apply_state(
            &registry,
            &[EntityStateEntry {
                id: u64::MAX,
                pos: [0.0, 64.0, 0.0],
                yaw: 0.0,
                health: 0.0,
                hurt: false,
                stack: Some(dto(&registry, "openblock:stone", 1, None)),
            }],
            now,
        );
        match entities.kind(u64::MAX) {
            Some(ReplicatedKind::Item(stack)) => assert_eq!(stack.count, 1, "partial pickup updates the count"),
            other => panic!("unexpected entity {other:?}"),
        }
        assert_eq!(entities.apply_despawn(&[u64::MAX, 1234], DespawnReason::PickedUp), 1);
        assert!(entities.is_empty());
        assert_eq!(entities.last_despawn(), Some((DespawnReason::PickedUp, 1)));
        entities.reset();
        assert_eq!(entities.last_despawn(), None);
        assert_eq!(entities.mob_visuals(now, 0.0).len(), 0);
    }

    #[test]
    fn entity_batches_and_live_capacity_are_bounded() {
        let registry = registry();
        let now = Instant::now();
        let kind = mob_kind("openblock:cow");
        let mut entities = ReplicatedEntities::default();

        // A hostile snapshot cannot grow the map past the batch cap.
        let oversized: Vec<EntitySnapshot> = (0..(MAX_ENTITY_BATCH as u64 + 32))
            .map(|index| snapshot(index + 1, kind.clone(), [0.0, 64.0, 0.0], 10.0))
            .collect();
        assert_eq!(entities.apply_snapshot(&registry, oversized, now), MAX_ENTITY_BATCH);
        assert_eq!(entities.len(), MAX_ENTITY_BATCH);

        // A stream of spawns stops at the live-map cap; updates still apply.
        let mut accepted = 0;
        for index in 0..(MAX_REPLICATED_ENTITIES as u64 + 16) {
            if entities.apply_spawn(
                &registry,
                snapshot(1_000_000 + index, kind.clone(), [0.0, 64.0, 0.0], 10.0),
                now,
            ) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, MAX_REPLICATED_ENTITIES - MAX_ENTITY_BATCH);
        assert_eq!(entities.len(), MAX_REPLICATED_ENTITIES);
        assert!(!entities.apply_spawn(&registry, snapshot(u64::MAX, kind.clone(), [0.0, 64.0, 0.0], 10.0), now));
        assert!(entities.apply_spawn(&registry, snapshot(1_000_000, kind.clone(), [1.0, 65.0, 1.0], 9.0), now));
        assert_eq!(entities.len(), MAX_REPLICATED_ENTITIES);

        // Only the first MAX_ENTITY_BATCH state entries are applied.
        let tracked = entities.ids();
        let untouched_health = entities.health(tracked[MAX_ENTITY_BATCH]);
        let states: Vec<EntityStateEntry> = tracked
            .iter()
            .map(|id| EntityStateEntry {
                id: *id,
                pos: [0.0, 64.0, 0.0],
                yaw: 0.0,
                health: 1.0,
                hurt: false,
                stack: None,
            })
            .collect();
        assert_eq!(entities.apply_state(&registry, &states, now), MAX_ENTITY_BATCH);
        assert_eq!(entities.health(tracked[MAX_ENTITY_BATCH - 1]), Some(1.0));
        assert_eq!(
            entities.health(tracked[MAX_ENTITY_BATCH]),
            untouched_health,
            "state entries past the batch cap are ignored"
        );

        // A huge despawn list cannot do more work than the map holds.
        let mut ids = entities.ids();
        ids.extend(std::iter::repeat_n(u64::MAX, MAX_REPLICATED_ENTITIES * 2));
        assert_eq!(
            entities.apply_despawn(&ids, DespawnReason::Despawned),
            MAX_REPLICATED_ENTITIES
        );
        assert!(entities.is_empty());
    }

    #[test]
    fn entity_raycast_targets_the_nearest_mob() {
        let registry = registry();
        let now = Instant::now();
        let mut entities = ReplicatedEntities::default();
        entities.apply_snapshot(
            &registry,
            vec![
                snapshot(5, mob_kind("openblock:cow"), [0.0, 64.0, -2.0], 10.0),
                snapshot(9, mob_kind("openblock:cow"), [0.0, 64.0, -4.0], 10.0),
            ],
            now,
        );
        let origin = WorldVec::new(0.0, 64.9, 0.0);
        let direction = WorldVec::new(0.0, 0.0, -1.0);
        assert_eq!(entities.raycast_mob(&registry, origin, direction, 6.0, now), Some(5));
        assert_eq!(entities.raycast_mob(&registry, origin, direction, 1.0, now), None);
    }

    #[test]
    fn remote_container_pick_merge_swap_and_output_rules() {
        let registry = registry();
        let mut hotbar = empty_hotbar();
        hotbar[0] = Some(stack(&registry, "openblock:stone", 5, None));
        hotbar[1] = Some(stack(&registry, "openblock:coal", 2, None));
        let mut containers = furnace_open(&registry);

        // Pick the whole input stack; no server transaction yet.
        let outcome = containers.click(
            &registry,
            &hotbar,
            SlotId::FurnaceInput,
            crate::containers::ClickButton::Left,
        );
        assert!(outcome.transfer.is_none());
        assert_eq!(containers.cursor().map(|cursor| cursor.source), Some(100));
        assert_eq!(containers.cursor().map(|cursor| cursor.stack.count), Some(2));
        // Shadow view hides the held stack in the input slot.
        let view = containers.snapshot(&hotbar).expect("view");
        assert!(view.stack(SlotId::FurnaceInput).is_none());

        // Clicking the source puts the preview back.
        let outcome = containers.click(
            &registry,
            &hotbar,
            SlotId::FurnaceInput,
            crate::containers::ClickButton::Left,
        );
        assert!(outcome.transfer.is_none());
        assert!(containers.cursor().is_none());

        // Right-click picks half (ceil) and placing one into the empty output
        // is refused: the output is take-only.
        containers.click(
            &registry,
            &hotbar,
            SlotId::FurnaceInput,
            crate::containers::ClickButton::Right,
        );
        assert_eq!(containers.cursor().map(|cursor| cursor.stack.count), Some(1));
        let refused = containers.click(
            &registry,
            &hotbar,
            SlotId::FurnaceOutput,
            crate::containers::ClickButton::Left,
        );
        assert!(refused.transfer.is_none());
        assert!(refused.message.is_some());

        // Put it back, then merge the furnace output into an empty hotbar slot.
        containers.click(
            &registry,
            &hotbar,
            SlotId::FurnaceInput,
            crate::containers::ClickButton::Left,
        );
        containers.open(
            9,
            WireContainerKind::Furnace,
            BlockPos::new(1, 2, 3),
            5,
            vec![
                Some(dto(&registry, "openblock:raw_iron", 1, None)),
                Some(dto(&registry, "openblock:coal", 2, None)),
                Some(dto(&registry, "openblock:iron_ingot", 1, None)),
            ],
            None,
            &registry,
        );
        containers.click(
            &registry,
            &hotbar,
            SlotId::FurnaceOutput,
            crate::containers::ClickButton::Left,
        );
        assert_eq!(containers.cursor().map(|cursor| cursor.source), Some(102));
        let outcome = containers.click(
            &registry,
            &hotbar,
            SlotId::Hotbar(4),
            crate::containers::ClickButton::Left,
        );
        let transfer = outcome.transfer.expect("output take");
        assert_eq!(
            transfer.to_client_message(),
            ob_protocol::ClientMessage::InventoryMove {
                from: 102,
                to: 4,
                count: 1,
                revision: Some(5),
            }
        );
        assert!(containers.cursor().is_none());
    }

    #[test]
    fn remote_container_merge_half_and_full_respects_stack_limits() {
        let registry = registry();
        let mut hotbar = empty_hotbar();
        hotbar[3] = Some(stack(&registry, "openblock:stone", 10, None));
        let mut containers = RemoteContainers::default();
        containers.open(
            1,
            WireContainerKind::Chest,
            BlockPos::new(0, 0, 0),
            2,
            vec![
                Some(dto(&registry, "openblock:stone", 63, None)),
                Some(dto(&registry, "openblock:dirt", 4, None)),
                None,
            ],
            None,
            &registry,
        );
        // Pick 10 stone from the hotbar, click the 63-stone chest stack: only
        // one fits, so the intent is count 1 and the cursor keeps 9.
        containers.click(
            &registry,
            &hotbar,
            SlotId::Hotbar(3),
            crate::containers::ClickButton::Left,
        );
        let outcome = containers.click(
            &registry,
            &hotbar,
            SlotId::Chest(0),
            crate::containers::ClickButton::Left,
        );
        assert_eq!(outcome.transfer.map(|transfer| transfer.count), Some(1));
        assert_eq!(containers.cursor().map(|cursor| cursor.stack.count), Some(9));
        // Clicking back on the source cancels the preview without a message.
        let outcome = containers.click(
            &registry,
            &hotbar,
            SlotId::Hotbar(3),
            crate::containers::ClickButton::Left,
        );
        assert!(outcome.transfer.is_none());
        assert!(containers.cursor().is_none());

        // Right-click placing one into an empty slot.
        containers.click(
            &registry,
            &hotbar,
            SlotId::Hotbar(3),
            crate::containers::ClickButton::Left,
        );
        let outcome = containers.click(
            &registry,
            &hotbar,
            SlotId::Chest(2),
            crate::containers::ClickButton::Right,
        );
        let transfer = outcome.transfer.expect("one item");
        assert_eq!((transfer.from, transfer.to, transfer.count), (3, 102, 1));
        assert_eq!(transfer.revision, Some(2));
        assert_eq!(containers.cursor().map(|cursor| cursor.stack.count), Some(9));

        // A different target requires the whole stack; a partial cursor is refused.
        let outcome = containers.click(
            &registry,
            &hotbar,
            SlotId::Chest(1),
            crate::containers::ClickButton::Left,
        );
        assert!(outcome.transfer.is_none());
        assert!(outcome.message.is_some());
    }

    #[test]
    fn remote_container_swap_and_stale_revision() {
        let registry = registry();
        let mut hotbar = empty_hotbar();
        hotbar[0] = Some(stack(&registry, "openblock:dirt", 4, None));
        let mut containers = RemoteContainers::default();
        containers.open(
            3,
            WireContainerKind::Chest,
            BlockPos::new(0, 0, 0),
            10,
            vec![Some(dto(&registry, "openblock:stone", 7, None))],
            None,
            &registry,
        );
        // Pick the whole dirt stack, then click the different stone stack:
        // the server swaps the two whole stacks.
        containers.click(
            &registry,
            &hotbar,
            SlotId::Hotbar(0),
            crate::containers::ClickButton::Left,
        );
        let outcome = containers.click(
            &registry,
            &hotbar,
            SlotId::Chest(0),
            crate::containers::ClickButton::Left,
        );
        let transfer = outcome.transfer.expect("swap");
        assert_eq!((transfer.from, transfer.to, transfer.count), (0, 100, 4));
        assert_eq!(
            containers.cursor().map(|cursor| cursor.stack.item),
            registry.item_id("openblock:stone")
        );
        assert_eq!(containers.cursor().map(|cursor| cursor.stack.count), Some(7));

        // A stale revision is ignored; a progress-only equal revision is accepted.
        let stale = containers.update(
            &registry,
            3,
            9,
            vec![Some(dto(&registry, "openblock:stone", 7, None))],
            None,
        );
        assert!(!stale.accepted);
        let accepted = containers.update(
            &registry,
            3,
            10,
            vec![Some(dto(&registry, "openblock:stone", 7, None))],
            None,
        );
        assert!(accepted.accepted);
        assert!(!accepted.slots_changed);
        assert!(!accepted.cursor_dropped, "a stale hotbar must not drop a swap cursor");

        // A window-sourced cursor is dropped when its authoritative slot no
        // longer contains it (for example another player took the stack).
        let mut containers = furnace_open(&registry);
        containers.click(
            &registry,
            &hotbar,
            SlotId::FurnaceInput,
            crate::containers::ClickButton::Left,
        );
        assert_eq!(containers.cursor().map(|cursor| cursor.source), Some(100));
        let dropped = containers.update(
            &registry,
            7,
            5,
            vec![None, Some(dto(&registry, "openblock:coal", 3, None)), None],
            None,
        );
        assert!(dropped.accepted && dropped.cursor_dropped);
        assert!(containers.cursor().is_none());
        assert!(containers.close(7));
        assert!(containers.window().is_none());
        assert!(!containers.close(7));
    }

    #[test]
    fn rejection_hints_cover_the_server_reason_catalogue() {
        let reasons = [
            "out_of_reach",
            "cooldown",
            "invalid_slot",
            "destination_full",
            "no_window",
            "stale_revision",
            "not_edible_or_full",
            "not_equippable",
            "not_dead",
            "no_target",
            "unknown_reason",
        ];
        for reason in reasons {
            let text = friendly_action_rejection("block_edit", reason);
            assert!(!text.is_empty(), "{reason}");
        }
        assert_eq!(
            friendly_action_rejection("block_edit", "out_of_reach"),
            "Too far away (block_edit)"
        );
        assert_eq!(
            friendly_action_rejection("inventory_move", "stale_revision"),
            "The container changed; try again (inventory_move)"
        );
    }
}

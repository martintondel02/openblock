//! Versioned client/server messages for the interim JSON protocol.
//!
//! Every message is an internally tagged JSON object with a `"type"` field
//! (`hello`, `chunk_request`, ...). Direction is implicit: a client only ever
//! sends [`ClientMessage`] and a server only ever sends [`ServerMessage`], so
//! the two enums may reuse variant names such as `Hello` and `Chat`.
//!
//! Numeric block ids are **session-local registry indices**. They are only
//! meaningful together with the block manifest and content fingerprint carried
//! by [`ServerMessage::Hello`]; see [`crate::block_manifest`] and
//! [`crate::content_fingerprint`].

use serde::{Deserialize, Serialize};

use crate::ProtocolError;
use crate::frame::MAX_FRAME_LEN;

/// The protocol version implemented by this crate.
///
/// Version 4 adds the 1.3 danger slice: `InteractEntity`, `InteractResult`,
/// `XpSync`, `Explosion`, projectile entity kinds, mob breeding/baby DTO fields
/// and charged-attack result fields. Versions 1–3 are rejected with
/// `protocol_mismatch` before any session state is exchanged. Framing, chunk and
/// light messages are unchanged.
pub const PROTOCOL_VERSION: u16 = 4;

/// Maximum number of bytes in a player name.
pub const MAX_NAME_LEN: usize = 32;
/// Maximum number of Unicode scalar values in one chat line.
pub const MAX_CHAT_LEN: usize = 256;
/// Maximum number of bytes in a server MOTD.
pub const MAX_MOTD_LEN: usize = 128;
/// Ticks in one full in-game day at 20 Hz (24,000 ticks = 20 minutes).
pub const DAY_LENGTH_TICKS: u64 = 24_000;
/// Maximum sections batched into one [`ServerMessage::LightUpdate`].
///
/// A dense section is at most 8 KiB before zstd/base64, so 64 sections stay
/// well inside the 2 MiB frame limit; the server sends at most one batch per
/// player per tick and applies a per-client rate limit on top.
pub const MAX_LIGHT_SECTIONS_PER_UPDATE: usize = 64;

/// One block type as advertised during the handshake.
///
/// `id` is the server's session-local registry index; `name` is the stable
/// namespaced resource id (for example `openblock:stone`). A client must build
/// its registry mapping from this list after validating
/// [`crate::ServerMessage::Hello::content_fingerprint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDescriptor {
    /// Session-local block index.
    pub id: u16,
    /// Stable namespaced block id.
    pub name: String,
}

/// One connected player, as sent in the initial roster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    /// Connection id, unique for the lifetime of the server process.
    pub id: u64,
    /// Player display name.
    pub name: String,
    /// Feet position in world space.
    pub pos: [f64; 3],
    /// Horizontal look angle in radians.
    pub yaw: f32,
    /// Vertical look angle in radians.
    pub pitch: f32,
}

/// Number of hotbar slots in a [`ServerMessage::InventorySync`].
pub const HOTBAR_SLOTS: usize = 9;
/// Number of main inventory slots in a [`ServerMessage::InventorySync`].
pub const MAIN_SLOTS: usize = 27;
/// Number of player inventory slots (`hotbar + main`).
pub const INVENTORY_SLOTS: usize = HOTBAR_SLOTS + MAIN_SLOTS;
/// Number of equipment (armor) slots in a [`ServerMessage::InventorySync`].
pub const EQUIPMENT_SLOTS: usize = 4;
/// First wire address of a main inventory slot.
pub const MAIN_SLOT_START: u16 = HOTBAR_SLOTS as u16;
/// First wire address of an equipment (armor) slot.
///
/// Player inventory slots are `0..36`, so the four equipment slots are
/// `36..40`. An open container window starts at [`SLOT_WINDOW_START`].
pub const EQUIPMENT_SLOT_START: u16 = INVENTORY_SLOTS as u16;
/// First address of an open container window in an
/// [`ClientMessage::InventoryMove`].
///
/// Player hotbar slots are `0..9`, main inventory slots are `9..36`, equipment
/// slots are `36..40` and an open window starts at `100`. A furnace exposes
/// input, fuel and output as `100..103`; a chest exposes its 27 slots as
/// `100..127`.
pub const SLOT_WINDOW_START: u16 = 100;
/// Number of slots in a furnace window.
pub const FURNACE_WINDOW_SLOTS: usize = 3;
/// Number of slots in a chest window.
pub const CHEST_WINDOW_SLOTS: usize = 27;

/// One item stack over the wire.
///
/// `item` is the stable namespaced resource id (`openblock:stone`), not a
/// session-local numeric id, so inventory data is meaningful without an item
/// manifest. `durability` is only present for tools and armor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemStackDto {
    /// Stable namespaced item resource id.
    pub item: String,
    /// Stack size, `1..=255` (the server clamps to the content max stack).
    pub count: u8,
    /// Remaining durability for tools and armor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durability: Option<u16>,
}

/// The kind of block entity an open window belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerKind {
    /// A furnace with input, fuel and output slots.
    Furnace,
    /// A chest with [`CHEST_WINDOW_SLOTS`] slots.
    Chest,
}

/// Furnace progress fields mirrored to a client with an open window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FurnaceProgress {
    /// Ticks left in the current burn (`0` means unlit).
    pub burn_remaining: u16,
    /// Total ticks of the current/last burn.
    pub burn_total: u16,
    /// Ticks already spent on the current smelt.
    pub cook_progress: u16,
}

/// The kind and payload of one replicated world entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntityKind {
    /// A mob species (stable resource id such as `openblock:zombie`).
    Mob {
        /// Stable mob species id.
        mob: String,
    },
    /// A dropped item entity with its current stack.
    Item {
        /// The dropped stack.
        stack: ItemStackDto,
    },
    /// A server-simulated arrow (1.3).
    Projectile {
        /// Stable projectile resource id (`openblock:arrow`).
        projectile: String,
    },
}

/// Complete state of one replicated entity, used by
/// [`ServerMessage::EntitySpawn`] and [`ServerMessage::EntitySnapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntitySnapshot {
    /// Entity id, unique among live mobs and items in the session.
    pub id: u64,
    /// Entity kind and payload.
    pub kind: EntityKind,
    /// Feet/base position in world space.
    pub pos: [f64; 3],
    /// Horizontal look angle in radians (zero for item entities).
    pub yaw: f32,
    /// Entity health (`0` for item entities and projectiles).
    pub health: f32,
    /// Whether the mob is currently in its hurt/flinch state.
    pub hurt: bool,
    /// Deterministic inherited colour variant (`0..=3`); `0` for items and
    /// projectiles.
    #[serde(default)]
    pub variant: u8,
    /// Whether a mob is a growing baby (0.5× scale, no drops); `false` for
    /// items and projectiles.
    #[serde(default)]
    pub baby: bool,
}

/// One changed entity inside a batched [`ServerMessage::EntityState`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityStateEntry {
    /// Entity id.
    pub id: u64,
    /// Updated feet/base position.
    pub pos: [f64; 3],
    /// Updated horizontal look angle.
    pub yaw: f32,
    /// Updated health.
    pub health: f32,
    /// Updated hurt/flinch state.
    pub hurt: bool,
    /// Updated colour variant.
    #[serde(default)]
    pub variant: u8,
    /// Updated baby state.
    #[serde(default)]
    pub baby: bool,
    /// Updated dropped stack for item entities (`None` for mobs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<ItemStackDto>,
}

/// One section's dense sky+RGB light words in a [`ServerMessage::LightUpdate`].
///
/// `encoding` is [`crate::LIGHT_ENCODING_RAW`] (little-endian `u16` words) or
/// [`crate::LIGHT_ENCODING_ZSTD`]; `format` is
/// [`crate::LIGHT_SECTION_FORMAT`]. Decode the payload with
/// [`crate::decode_light_words`]. Words are in canonical `LocalPos::from_index`
/// order, exactly like the dense light encoding of a chunk snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LightSectionDto {
    /// Section X coordinate (world section grid).
    pub cx: i32,
    /// Section Y coordinate (world section grid).
    pub cy: i32,
    /// Section Z coordinate (world section grid).
    pub cz: i32,
    /// Payload format; must be [`crate::LIGHT_SECTION_FORMAT`].
    pub format: u8,
    /// Payload encoding, raw or zstd.
    pub encoding: u8,
    /// Encoded dense light words.
    #[serde(with = "crate::base64::base64_bytes")]
    pub data: std::vec::Vec<u8>,
}

/// Why the server removed an entity from a client's interest set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DespawnReason {
    /// A mob was killed by a player or daylight burn.
    Killed,
    /// A dropped item was picked up.
    PickedUp,
    /// The entity despawned by age, distance or the void.
    Despawned,
    /// The entity left this client's interest radius.
    LeftInterest,
}

/// A message sent by a client to the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// First message on a connection; must be sent exactly once.
    Hello {
        /// The client's [`PROTOCOL_VERSION`].
        protocol: u16,
        /// Requested player name.
        name: String,
    },
    /// Ask for a specific chunk column (for example after a reconnect or an
    /// edit near the edge of the streamed area).
    ChunkRequest {
        /// Chunk X coordinate.
        cx: i32,
        /// Chunk Z coordinate.
        cz: i32,
    },
    /// Client-authoritative movement update (feet position plus look angles).
    Move {
        /// Feet position in world space.
        pos: [f64; 3],
        /// Horizontal look angle in radians.
        yaw: f32,
        /// Vertical look angle in radians.
        pitch: f32,
    },
    /// Request an authoritative block edit. `block` is a session-local block
    /// id; `0` means break.
    ///
    /// In protocol v3+ this is also the survival break/place intent: the server
    /// resolves loot, block-entity contents, tool wear and placement
    /// consumption from the sending network player's authoritative state.
    BlockEdit {
        /// Block position.
        pos: [i32; 3],
        /// Replacement block id (`0` = air).
        block: u16,
    },
    /// Selects a hotbar slot (`0..9`) for the sending player.
    SelectSlot {
        /// Hotbar slot index.
        slot: u8,
    },
    /// Interacts with a block: opens a furnace or chest window, or uses the
    /// selected item on it (hoe on grass/dirt, seeds on farmland, harvest a
    /// crop, sleep in a bed). The server validates distance, support, light and
    /// item state itself; nothing in this message is trusted beyond `pos`.
    UseBlock {
        /// Block position.
        pos: [i32; 3],
    },
    /// Requests sleep in the bed at `pos` (foot or head block).
    ///
    /// Equivalent to [`ClientMessage::UseBlock`] on a bed for older bed clients; the
    /// explicit variant lets a client express intent without also triggering a
    /// container interaction. The server checks reach, the sleep window,
    /// obstruction and the other players' sleep state.
    SleepRequest {
        /// Foot or head block position of the bed.
        pos: [i32; 3],
    },
    /// Leaves a bed or cancels a pending sleep fast-forward.
    Wake,
    /// Melee attack intent.
    Attack {
        /// Explicit mob entity id. When omitted/`null`, the server raycasts
        /// from the player's eye along its look direction.
        #[serde(default)]
        target: Option<u64>,
    },
    /// Feeds/uses the selected item on one mob (1.3 breeding).
    ///
    /// The server validates reach (3 m), species food, love/cooldown state and
    /// the area cap; nothing in this message is trusted beyond `id`.
    InteractEntity {
        /// Mob entity id.
        id: u64,
    },
    /// Moves items between player, equipment and open-window slots.
    ///
    /// Addresses: hotbar `0..9`, main inventory `9..36`, equipment `36..40`,
    /// open window [`SLOT_WINDOW_START`] and above. `revision` should echo the
    /// latest `ContainerOpen`/`ContainerUpdate` revision when a window slot is
    /// involved; a present stale revision is rejected.
    InventoryMove {
        /// Source slot address.
        from: u16,
        /// Destination slot address.
        to: u16,
        /// Number of items to move, at least one.
        count: u8,
        /// Latest window revision seen by the client.
        #[serde(default)]
        revision: Option<u64>,
    },
    /// Eats one item from the selected hotbar slot.
    Eat,
    /// Equips the armor piece in one hotbar slot.
    Equip {
        /// Hotbar slot index.
        hotbar_slot: u8,
    },
    /// Respawns after death at the server spawn point.
    Respawn,
    /// Closes the sending player's open container window.
    ContainerClose,
    /// A chat line.
    Chat {
        /// Plain text, at most [`MAX_CHAT_LEN`] scalar values.
        text: String,
    },
    /// Round-trip latency probe.
    Ping {
        /// Opaque nonce echoed by [`ServerMessage::Pong`].
        id: u32,
    },
    /// Voluntary clean disconnect.
    Disconnect,
}

/// A message sent by the server to a client.
///
/// The `inventory_sync` variant embeds two fixed-size slot arrays, which makes
/// it larger than the other variants. Messages are serialised immediately and
/// not stored in bulk, so the size is irrelevant in practice.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Handshake reply with world metadata and the content manifest.
    Hello {
        /// The server's [`PROTOCOL_VERSION`].
        protocol: u16,
        /// Human-readable world directory name.
        world_name: String,
        /// World seed.
        seed: u64,
        /// Recommended spawn position (feet).
        spawn: [f64; 3],
        /// Registry fingerprint ([`crate::content_fingerprint`]).
        content_fingerprint: u64,
        /// Server tick at which the player joined.
        tick: u64,
        /// Ticks per in-game day (currently [`DAY_LENGTH_TICKS`]).
        day_ticks: u64,
        /// Configured player cap.
        max_players: u16,
        /// Server message of the day (plain text).
        motd: String,
        /// Complete block mapping in ascending id order.
        blocks: Vec<BlockDescriptor>,
    },
    /// A player joined. Also sent for the receiving client itself.
    Join {
        /// Connection id.
        id: u64,
        /// Player display name.
        name: String,
    },
    /// A player left.
    Leave {
        /// Connection id.
        id: u64,
    },
    /// Complete roster sent once immediately after the server's `Hello`.
    PlayerList {
        /// Every connected player, including the receiving client.
        players: Vec<PlayerSnapshot>,
    },
    /// Authoritative snapshot of one player.
    PlayerState {
        /// Connection id.
        id: u64,
        /// Player display name.
        name: String,
        /// Feet position in world space.
        pos: [f64; 3],
        /// Horizontal look angle in radians.
        yaw: f32,
        /// Vertical look angle in radians.
        pitch: f32,
    },
    /// One encoded chunk column. `bytes` is the payload produced by
    /// [`crate::encode_chunk`] (base64 in JSON).
    ChunkData {
        /// Chunk X coordinate.
        cx: i32,
        /// Chunk Z coordinate.
        cz: i32,
        /// Opaque chunk payload; decode with [`crate::decode_chunk`].
        #[serde(with = "crate::base64::base64_bytes")]
        bytes: Vec<u8>,
    },
    /// An accepted authoritative block edit, broadcast to every client.
    BlockEdit {
        /// Block position.
        pos: [i32; 3],
        /// New block id (`0` = air).
        block: u16,
        /// Connection id of the player that caused the edit.
        player: u64,
    },
    /// Authoritative vitals and armor defense of the receiving player.
    PlayerVitals {
        /// Health points, `0..=20`.
        health: f32,
        /// Hunger points, `0..=20`.
        hunger: f32,
        /// Effective armor defense points (`0..=20`).
        armor_defense: u8,
    },
    /// Authoritative player XP for the receiving player (1.3).
    ///
    /// `progress` is the XP inside the current level and `required` the points
    /// needed to reach the next level. The server awards XP directly; no orb
    /// entities exist in 1.3.
    XpSync {
        /// XP points inside the current level.
        points: u32,
        /// Current XP level.
        level: u32,
        /// Points already accumulated toward the next level.
        progress: u32,
        /// Points required to reach the next level.
        required: u32,
    },
    /// Complete inventory, selected slot and equipment of the receiving
    /// player.
    ///
    /// The hotbar and main arrays map to wire addresses `0..9` and `9..36`
    /// respectively; both are always present, including empty stacks. The
    /// equipment array maps to addresses `36..40`.
    InventorySync {
        /// The nine hotbar slots; `null` is empty.
        hotbar: [Option<ItemStackDto>; HOTBAR_SLOTS],
        /// The 27 main inventory slots; `null` is empty.
        main: [Option<ItemStackDto>; MAIN_SLOTS],
        /// Selected hotbar slot, `0..9`.
        selected: u8,
        /// The four armor slots (helmet, chestplate, leggings, boots).
        equipment: [Option<ItemStackDto>; EQUIPMENT_SLOTS],
    },
    /// The receiving player selected a different hotbar slot.
    SelectedSlot {
        /// Selected hotbar slot, `0..9`.
        slot: u8,
    },
    /// Opens a container window for the receiving player.
    ContainerOpen {
        /// Window id, unique per server process.
        window: u32,
        /// Furnace or chest.
        kind: ContainerKind,
        /// Block position of the container.
        pos: [i32; 3],
        /// Content revision for stale-transaction checks.
        revision: u64,
        /// Window slots (`3` for a furnace, `27` for a chest).
        slots: Vec<Option<ItemStackDto>>,
        /// Furnace progress; `None` for a chest.
        furnace: Option<FurnaceProgress>,
    },
    /// Authoritative update of the receiving player's open window.
    ContainerUpdate {
        /// Window id from the matching `ContainerOpen`.
        window: u32,
        /// New content revision.
        revision: u64,
        /// Window slots.
        slots: Vec<Option<ItemStackDto>>,
        /// Furnace progress; `None` for a chest.
        furnace: Option<FurnaceProgress>,
    },
    /// The receiving player's open window closed.
    ContainerClose {
        /// Window id.
        window: u32,
        /// Short machine-readable reason (`closed`, `block_removed`,
        /// `out_of_range`, `death`, `replaced`).
        reason: String,
    },
    /// One newly visible mob, item or projectile entity.
    EntitySpawn {
        /// Entity id.
        id: u64,
        /// Entity kind and payload.
        kind: EntityKind,
        /// Feet/base position in world space.
        pos: [f64; 3],
        /// Horizontal look angle in radians.
        yaw: f32,
        /// Entity health (`0` for item and projectile entities).
        health: f32,
        /// Whether the mob is currently in its hurt/flinch state.
        hurt: bool,
        /// Colour variant (`0..=3`; 0 for items/projectiles).
        #[serde(default)]
        variant: u8,
        /// Whether a mob is a growing baby.
        #[serde(default)]
        baby: bool,
    },
    /// Initial set of visible entities, sent once during the handshake.
    EntitySnapshot {
        /// Entities inside the joining player's interest radius.
        entities: Vec<EntitySnapshot>,
    },
    /// Batched changed entity states, sorted by ascending id.
    EntityState {
        /// Changed entities only.
        entities: Vec<EntityStateEntry>,
    },
    /// Entities removed from the receiving client's interest set.
    EntityDespawn {
        /// Removed entity ids.
        ids: Vec<u64>,
        /// Why they were removed.
        reason: DespawnReason,
    },
    /// One damaging hit against the receiving player.
    DamageEvent {
        /// Damage actually removed from health after armor.
        amount: f32,
        /// Short cause (`fall`, `starvation`, `mob`, ...).
        cause: String,
    },
    /// The receiving player died. Actions are ignored until `Respawn`.
    Death {
        /// Short cause (`fall`, `starvation`, `mob`, ...).
        cause: String,
    },
    /// The receiving player respawned.
    Respawn {
        /// New feet position.
        pos: [f64; 3],
        /// Restored health.
        health: f32,
        /// Restored hunger.
        hunger: f32,
    },
    /// Result of one melee attack intent.
    AttackResult {
        /// Target mob id.
        target: u64,
        /// Whether the hit landed.
        hit: bool,
        /// Health removed by this hit.
        damage: f32,
        /// Whether the mob died.
        killed: bool,
        /// Server-computed attack charge (`0..=1`).
        charge: f32,
        /// Whether the hit was a falling critical (1.5×).
        critical: bool,
        /// Milliseconds still needed before another charged hit is allowed.
        cooldown_remaining_ms: u32,
    },
    /// Result of one mob interaction (feeding/breeding) intent.
    InteractResult {
        /// Target mob id.
        id: u64,
        /// Whether the interaction succeeded.
        accepted: bool,
        /// Short machine-readable rejection reason (`wrong_food`, `cooldown`,
        /// `cap`, ...); empty on success.
        reason: String,
    },
    /// A creeper-like explosion happened (1.3 visual/audio cue).
    ///
    /// Block removals arrive as ordinary [`ServerMessage::BlockEdit`] frames;
    /// this message exists so a client can flash/shake or play a one-shot sound
    /// without inferring an explosion from an edit burst.
    Explosion {
        /// Centre position.
        pos: [f64; 3],
        /// Radius in metres.
        radius: f32,
    },
    /// A gameplay action was rejected; the reason is for logs/UI hints.
    ActionRejected {
        /// Short action name (`block_edit`, `use_block`, `inventory_move`, ...).
        action: String,
        /// Short machine-readable reason.
        reason: String,
    },
    /// Dense light patches for sections whose stored light changed after a
    /// server edit or relight.
    ///
    /// The server batches changes per tick, sends at most
    /// [`MAX_LIGHT_SECTIONS_PER_UPDATE`] sections in one message, skips players
    /// outside the section's interest radius and applies a per-client rate
    /// limit. Clients replace the stored light of each section wholesale.
    LightUpdate {
        /// Server tick at which the batch was produced.
        tick: u64,
        /// Changed sections. Empty batches are never sent.
        sections: std::vec::Vec<LightSectionDto>,
    },
    /// Authoritative world clock, used by the client to render the day/night
    /// cycle and to follow a sleep fast-forward.
    TimeSync {
        /// Authoritative ticks since world creation.
        tick: u64,
        /// `tick % day_ticks` (the day phase).
        day_phase: u64,
        /// Ticks in one full day (currently [`DAY_LENGTH_TICKS`]).
        day_ticks: u64,
    },
    /// Sleep state of the receiving player and the rest of the session.
    ///
    /// `sleepers` is sorted ascending. `all_sleeping` is true only when every
    /// eligible (connected, alive, non-spectator) player is in a bed. The
    /// server still waits for the 100-tick sunrise fast-forward; the field tells
    /// the client whether the skip is running. `skip_remaining_ticks` counts the
    /// fast-forward ticks left (`0` when no skip is active).
    SleepState {
        /// Whether the receiving player is asleep.
        sleeping: bool,
        /// Receiving player's bed foot position, if asleep.
        bed: Option<[i32; 3]>,
        /// Connected players currently asleep, ascending.
        sleepers: std::vec::Vec<u64>,
        /// Whether every eligible player is asleep.
        all_sleeping: bool,
        /// Fast-forward ticks remaining in the 100-tick sunrise skip.
        skip_remaining_ticks: u32,
    },
    /// A relayed chat line.
    Chat {
        /// Display name of the sender (`server` for console messages).
        from: String,
        /// Plain text.
        text: String,
    },
    /// Reply to [`ClientMessage::Ping`].
    Pong {
        /// Echoed nonce.
        id: u32,
    },
    /// The server is closing the connection. The reason is a short machine
    /// readable code, optionally followed by `: detail`.
    Disconnect {
        /// Reason code and optional detail.
        reason: String,
    },
}

/// Validates a requested player name.
///
/// Names are 1..=[`MAX_NAME_LEN`] ASCII scalars from `[A-Za-z0-9_.-]`. The
/// server applies the same rule to every `Hello` and rejects invalid names
/// with a `Disconnect { reason: "invalid_name: ..." }`.
pub fn validate_player_name(name: &str) -> Result<(), ProtocolError> {
    if name.is_empty() {
        return Err(ProtocolError::InvalidName("name is empty"));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(ProtocolError::InvalidName("name is longer than 32 bytes"));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ProtocolError::InvalidName(
            "name may only contain ASCII letters, digits, '_', '-' and '.'",
        ));
    }
    Ok(())
}

/// Validates and normalises one chat line.
///
/// The returned text is trimmed of leading/trailing whitespace and is
/// guaranteed non-empty, at most [`MAX_CHAT_LEN`] scalar values and free of
/// control characters.
pub fn validate_chat_text(text: &str) -> Result<String, ProtocolError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ProtocolError::InvalidChat("chat text is empty"));
    }
    if text.chars().count() > MAX_CHAT_LEN {
        return Err(ProtocolError::InvalidChat("chat text is too long"));
    }
    if text.chars().any(char::is_control) {
        return Err(ProtocolError::InvalidChat("chat text contains control characters"));
    }
    Ok(text.to_string())
}

/// Truncates and sanitises a MOTD for `ServerMessage::Hello`.
///
/// Control characters become spaces, the text is trimmed and at most
/// [`MAX_MOTD_LEN`] bytes are kept (on a UTF-8 boundary).
pub fn sanitise_motd(motd: &str) -> String {
    let mut text: String = motd
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect();
    text = text.trim().to_string();
    if text.len() > MAX_MOTD_LEN {
        let mut end = MAX_MOTD_LEN;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Builds the server's block manifest from a registry.
///
/// Entries are in ascending block-id order and use the stable namespaced
/// resource ids, so the result is deterministic across processes.
pub fn block_manifest(registry: &ob_content::Registry) -> Vec<BlockDescriptor> {
    registry
        .iter()
        .map(|(id, block)| BlockDescriptor {
            id: id.0,
            name: block.id.to_string(),
        })
        .collect()
}

/// Stable fingerprint of a registry, identical to the world-save fingerprint.
pub fn content_fingerprint(registry: &ob_content::Registry) -> u64 {
    ob_world::registry_fingerprint(registry)
}

/// Whether a handshake manifest describes exactly `registry`.
///
/// This checks the block count, each id and each name. Clients should also
/// compare [`content_fingerprint`] before accepting the manifest.
pub fn manifest_matches(blocks: &[BlockDescriptor], registry: &ob_content::Registry) -> bool {
    blocks.len() == registry.len()
        && blocks.iter().enumerate().all(|(index, descriptor)| {
            u16::try_from(index).ok() == Some(descriptor.id)
                && registry.id(&descriptor.name) == Some(ob_content::BlockId(descriptor.id))
        })
}

/// Returns the maximum frame size accepted by [`crate::decode_frame`] helpers
/// that do not take an explicit limit.
pub const fn default_max_frame_len() -> usize {
    MAX_FRAME_LEN
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_content::Registry;

    #[test]
    fn base_registry_manifest_is_deterministic_and_matches() {
        let registry = Registry::base();
        let first = block_manifest(&registry);
        let second = block_manifest(&registry);
        assert_eq!(first, second);
        assert_eq!(first.len(), registry.len());
        assert_eq!(first[0].id, 0);
        assert_eq!(first[0].name, "openblock:air");
        assert!(manifest_matches(&first, &registry));
        assert_eq!(
            content_fingerprint(&registry),
            ob_world::registry_fingerprint(&registry)
        );

        let mut broken = first;
        broken[0].name = "openblock:not_air".to_string();
        assert!(!manifest_matches(&broken, &registry));
    }

    #[test]
    fn name_validation_rejects_hostile_input() {
        assert!(validate_player_name("alice").is_ok());
        assert!(validate_player_name("Player-1.test_2").is_ok());
        for invalid in ["", "al ice", "alice\n", "ælice", &"a".repeat(MAX_NAME_LEN + 1)] {
            assert!(validate_player_name(invalid).is_err(), "{invalid:?} must be rejected");
        }
    }

    #[test]
    fn chat_validation_trims_and_rejects_control_characters() {
        assert_eq!(validate_chat_text("  hello  ").expect("valid"), "hello");
        assert!(validate_chat_text("   ").is_err());
        assert!(validate_chat_text("hello\nworld").is_err());
        assert!(validate_chat_text(&"x".repeat(MAX_CHAT_LEN + 1)).is_err());
        assert!(validate_chat_text(&"x".repeat(MAX_CHAT_LEN)).is_ok());
    }

    fn sample_stack() -> ItemStackDto {
        ItemStackDto {
            item: "openblock:wooden_pickaxe".to_string(),
            count: 1,
            durability: Some(59),
        }
    }

    #[test]
    fn survival_client_messages_roundtrip() {
        let messages = vec![
            ClientMessage::SelectSlot { slot: 4 },
            ClientMessage::UseBlock { pos: [1, 2, 3] },
            ClientMessage::Attack { target: Some(7) },
            ClientMessage::Attack { target: None },
            ClientMessage::InventoryMove {
                from: 0,
                to: 100,
                count: 3,
                revision: Some(9),
            },
            ClientMessage::InventoryMove {
                from: 36,
                to: 2,
                count: 1,
                revision: None,
            },
            ClientMessage::InventoryMove {
                from: 9,
                to: 35,
                count: 64,
                revision: None,
            },
            ClientMessage::Eat,
            ClientMessage::Equip { hotbar_slot: 2 },
            ClientMessage::Respawn,
            ClientMessage::ContainerClose,
            ClientMessage::SleepRequest { pos: [4, 65, -7] },
            ClientMessage::Wake,
        ];
        for message in messages {
            let json = serde_json::to_string(&message).expect("serialise client message");
            let decoded: ClientMessage = serde_json::from_str(&json).expect("decode client message");
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn survival_server_messages_roundtrip() {
        let stack = sample_stack();
        let furnace = FurnaceProgress {
            burn_remaining: 100,
            burn_total: 1_600,
            cook_progress: 42,
        };
        let entity = EntitySnapshot {
            id: 11,
            kind: EntityKind::Item { stack: stack.clone() },
            pos: [1.0, 2.0, 3.0],
            yaw: 0.0,
            health: 0.0,
            hurt: false,
            variant: 0,
            baby: false,
        };
        let messages = vec![
            ServerMessage::PlayerVitals {
                health: 17.5,
                hunger: 12.0,
                armor_defense: 7,
            },
            ServerMessage::InventorySync {
                hotbar: std::array::from_fn(|_| None),
                main: std::array::from_fn(|slot| (slot == 0).then(|| stack.clone())),
                selected: 3,
                equipment: std::array::from_fn(|_| None),
            },
            ServerMessage::SelectedSlot { slot: 3 },
            ServerMessage::ContainerOpen {
                window: 4,
                kind: ContainerKind::Furnace,
                pos: [10, 64, -2],
                revision: 3,
                slots: vec![Some(stack.clone()), None, None],
                furnace: Some(furnace),
            },
            ServerMessage::ContainerUpdate {
                window: 4,
                revision: 4,
                slots: vec![None, None, Some(stack.clone())],
                furnace: Some(furnace),
            },
            ServerMessage::ContainerClose {
                window: 4,
                reason: "closed".to_string(),
            },
            ServerMessage::EntitySpawn {
                id: entity.id,
                kind: entity.kind.clone(),
                pos: entity.pos,
                yaw: entity.yaw,
                health: entity.health,
                hurt: entity.hurt,
                variant: entity.variant,
                baby: entity.baby,
            },
            ServerMessage::EntitySnapshot {
                entities: vec![entity.clone()],
            },
            ServerMessage::EntityState {
                entities: vec![EntityStateEntry {
                    id: 11,
                    pos: [1.5, 2.0, 3.5],
                    yaw: 0.25,
                    health: 9.0,
                    hurt: true,
                    variant: 2,
                    baby: true,
                    stack: Some(stack.clone()),
                }],
            },
            ServerMessage::EntityDespawn {
                ids: vec![11, 12],
                reason: DespawnReason::PickedUp,
            },
            ServerMessage::DamageEvent {
                amount: 3.0,
                cause: "fall".to_string(),
            },
            ServerMessage::Death {
                cause: "mob".to_string(),
            },
            ServerMessage::Respawn {
                pos: [0.5, 70.0, 0.5],
                health: 20.0,
                hunger: 20.0,
            },
            ServerMessage::AttackResult {
                target: 7,
                hit: true,
                damage: 4.0,
                killed: false,
                charge: 0.95,
                critical: false,
                cooldown_remaining_ms: 31,
            },
            ServerMessage::InteractResult {
                id: 7,
                accepted: true,
                reason: String::new(),
            },
            ServerMessage::XpSync {
                points: 4,
                level: 3,
                progress: 4,
                required: 13,
            },
            ServerMessage::Explosion {
                pos: [1.0, 64.0, -2.0],
                radius: 3.5,
            },
            ServerMessage::ActionRejected {
                action: "block_edit".to_string(),
                reason: "out_of_reach".to_string(),
            },
            ServerMessage::LightUpdate {
                tick: 42,
                sections: vec![
                    crate::LightSectionDto {
                        cx: 0,
                        cy: 4,
                        cz: -1,
                        format: crate::LIGHT_SECTION_FORMAT,
                        encoding: crate::LIGHT_ENCODING_RAW,
                        data: crate::encode_light_words(&vec![0x0F5A; crate::LIGHT_CELLS])
                            .expect("light words")
                            .1,
                    },
                    crate::LightSectionDto {
                        cx: 2,
                        cy: -2,
                        cz: 3,
                        format: crate::LIGHT_SECTION_FORMAT,
                        encoding: crate::LIGHT_ENCODING_ZSTD,
                        data: crate::encode_light_words(&vec![0u16; crate::LIGHT_CELLS])
                            .expect("light words")
                            .1,
                    },
                ],
            },
            ServerMessage::TimeSync {
                tick: 24_500,
                day_phase: 500,
                day_ticks: DAY_LENGTH_TICKS,
            },
            ServerMessage::SleepState {
                sleeping: true,
                bed: Some([10, 64, -3]),
                sleepers: vec![1, 2, 3],
                all_sleeping: true,
                skip_remaining_ticks: 63,
            },
        ];
        for message in messages {
            let json = serde_json::to_string(&message).expect("serialise server message");
            let decoded: ServerMessage = serde_json::from_str(&json).expect("decode server message");
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn v3_payloads_use_stable_names_and_tags() {
        let item = EntitySnapshot {
            id: 1,
            kind: EntityKind::Item { stack: sample_stack() },
            pos: [0.0; 3],
            yaw: 0.0,
            health: 0.0,
            hurt: false,
            variant: 0,
            baby: false,
        };
        let json = serde_json::to_value(&item).expect("json");
        assert_eq!(json["kind"]["kind"], "item");
        assert_eq!(json["kind"]["stack"]["item"], "openblock:wooden_pickaxe");
        assert_eq!(json["kind"]["stack"]["durability"], 59);
        let mob = EntityKind::Mob {
            mob: "openblock:zombie".to_string(),
        };
        let json = serde_json::to_value(&mob).expect("json");
        assert_eq!(json["kind"], "mob");
        assert_eq!(json["mob"], "openblock:zombie");

        // A revision omitted by an older UI is accepted (no stale check).
        let move_without_revision: ClientMessage =
            serde_json::from_str(r#"{"type":"inventory_move","from":0,"to":100,"count":1}"#).expect("legacy move");
        assert_eq!(
            move_without_revision,
            ClientMessage::InventoryMove {
                from: 0,
                to: 100,
                count: 1,
                revision: None,
            }
        );
        assert_eq!(SLOT_WINDOW_START, 100);
        assert_eq!(MAIN_SLOT_START, 9);
        assert_eq!(EQUIPMENT_SLOT_START, 36);
        assert_eq!(HOTBAR_SLOTS, 9);
        assert_eq!(MAIN_SLOTS, 27);
        assert_eq!(INVENTORY_SLOTS, 36);
        assert_eq!(EQUIPMENT_SLOTS, 4);
        assert_eq!(EQUIPMENT_SLOT_START + EQUIPMENT_SLOTS as u16, 40);
        assert_eq!(PROTOCOL_VERSION, 4);
        assert_eq!(MAX_LIGHT_SECTIONS_PER_UPDATE, 64);
    }

    #[test]
    fn light_and_sleep_wire_tags_are_stable() {
        let light = ServerMessage::LightUpdate {
            tick: 7,
            sections: vec![crate::LightSectionDto {
                cx: 1,
                cy: 2,
                cz: 3,
                format: crate::LIGHT_SECTION_FORMAT,
                encoding: crate::LIGHT_ENCODING_RAW,
                data: vec![0, 0, 1, 0],
            }],
        };
        let json = serde_json::to_value(&light).expect("json");
        assert_eq!(json["type"], "light_update");
        assert_eq!(json["sections"][0]["cx"], 1);
        assert_eq!(json["sections"][0]["format"], 1);

        let sleep = serde_json::to_value(&ClientMessage::SleepRequest { pos: [1, 2, 3] }).expect("json");
        assert_eq!(sleep["type"], "sleep_request");
        let wake = serde_json::to_value(&ClientMessage::Wake).expect("json");
        assert_eq!(wake["type"], "wake");
        let state = serde_json::to_value(&ServerMessage::SleepState {
            sleeping: false,
            bed: None,
            sleepers: vec![],
            all_sleeping: false,
            skip_remaining_ticks: 0,
        })
        .expect("json");
        assert_eq!(state["type"], "sleep_state");
        let time = serde_json::to_value(&ServerMessage::TimeSync {
            tick: 1,
            day_phase: 1,
            day_ticks: DAY_LENGTH_TICKS,
        })
        .expect("json");
        assert_eq!(time["type"], "time_sync");
    }

    #[test]
    fn motd_is_sanitised_and_bounded() {
        assert_eq!(sanitise_motd("  welcome\nto the server  "), "welcome to the server");
        assert_eq!(sanitise_motd(&"m".repeat(MAX_MOTD_LEN + 10)).len(), MAX_MOTD_LEN);
        let unicode = format!("{}é", "x".repeat(MAX_MOTD_LEN - 1));
        let sanitised = sanitise_motd(&unicode);
        assert!(sanitised.len() <= MAX_MOTD_LEN);
        assert!(sanitised.is_char_boundary(sanitised.len()));
    }
}

#[cfg(test)]
mod hostile_json_tests {
    use super::*;

    #[test]
    fn duplicate_keys_and_hostile_numbers_are_rejected() {
        for payload in [
            r#"{"type":"ping","id":1,"id":2}"#,
            r#"{"type":"hello","protocol":3,"name":"a","name":"b"}"#,
            r#"{"type":"ping","type":"ping","id":1}"#,
            r#"{"type":"ping","id":-1}"#,
            r#"{"type":"ping","id":4294967296}"#,
            r#"{"type":"ping","id":1e400}"#,
            r#"{"type":"ping","id":NaN}"#,
            r#"{"type":"chunk_request","cx":2147483648,"cz":0}"#,
            r#"{"type":"ping","id":1}garbage"#,
        ] {
            assert!(
                serde_json::from_str::<ClientMessage>(payload).is_err(),
                "hostile payload was accepted: {payload}"
            );
        }
        // Duplicate keys must not silently select the last value.
        assert!(serde_json::from_str::<ServerMessage>(r#"{"type":"pong","id":1,"id":2}"#).is_err());
    }

    #[test]
    fn deeply_nested_json_is_bounded_by_the_recursion_limit() {
        let mut payload = String::from(r#"{"type":"ping","id":1,"ignored":"#);
        payload.push_str(&"[".repeat(512));
        payload.push('0');
        payload.push_str(&"]".repeat(512));
        payload.push('}');
        assert!(serde_json::from_str::<ClientMessage>(&payload).is_err());
    }
}

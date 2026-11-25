//! The OpenBlock interim wire protocol v4: JSON messages and chunk snapshots.
//!
//! **Status: implemented for the 1.0 self-hosted multiplayer milestone.** This
//! crate is the shared contract between the dedicated server
//! (`ob_server::net`) and the future client connect mode. The long-term design
//! in `docs/spec/18-networking-multiplayer.md` uses QUIC, TLS and a bit-packed
//! binary catalog; this interim protocol deliberately uses plain TCP and JSON
//! so the foundation is testable, inspectable and self-hostable without adding
//! a transport dependency. It is not encrypted and not suitable for exposure to
//! the public internet as-is.
//!
//! # Framing
//!
//! Every frame is a `u32` big-endian payload length followed by UTF-8 JSON.
//! [`MAX_FRAME_LEN`] bounds both the default encoder and every decoder used by
//! the server. Malformed, truncated, oversized and invalid-UTF-8 frames return
//! [`ProtocolError`], never panic.
//!
//! # Messages
//!
//! [`ClientMessage`] and [`ServerMessage`] are internally tagged enums (the
//! tag is `"type"`, values are snake_case). See their variant documentation for
//! the exact catalog. Direction is implied by which side of a connection sends
//! the frame.
//!
//! A server handshake is:
//!
//! 1. client [`ClientMessage::Hello`] (protocol + name),
//! 2. server [`ServerMessage::Hello`] (world metadata, content fingerprint and
//!    the complete block manifest),
//! 3. server [`ServerMessage::Join`] for the new player and
//!    [`ServerMessage::PlayerList`] with every connected player,
//! 4. existing players receive [`ServerMessage::Join`],
//! 5. the server sends the new player's [`ServerMessage::InventorySync`],
//!    [`ServerMessage::PlayerVitals`], [`ServerMessage::SelectedSlot`] and an
//!    [`ServerMessage::EntitySnapshot`] of visible mobs/items,
//! 6. chunk snapshots stream from the server and player state is broadcast at
//!    10 Hz.
//!
//! # Survival state (v3)
//!
//! Protocol v3 first made the server authoritative for network-player survival (v4 keeps it):
//! vitals (`PlayerVitals`), the 36-slot player inventory (nine hotbar plus 27
//! main stacks) and four armor slots (`InventorySync`/`SelectedSlot`), furnace
//! and chest windows
//! (`ContainerOpen`/`ContainerUpdate`/`ContainerClose`), damage/death/respawn
//! (`DamageEvent`/`Death`/`Respawn`) and mob/item entity replication
//! (`EntitySpawn`/`EntitySnapshot`/`EntityState`/`EntityDespawn`). Clients send
//! intents (`SelectSlot`, `UseBlock`, `Attack`, `InventoryMove`, `Eat`,
//! `Equip`, `Respawn`, `ContainerClose`) and never write authoritative state
//! directly.
//!
//! Slot addresses used by [`ClientMessage::InventoryMove`]: hotbar `0..9`,
//! main inventory `9..36`, equipment `36..40`, open window `100..`
//! ([`SLOT_WINDOW_START`]). See
//! [`ItemStackDto`], [`ContainerKind`], [`EntityKind`] and [`DespawnReason`] for
//! the payload shapes.
//!
//! # Light and sleep (still v3)
//!
//! The 1.2 / 1.3 gameplay slices add additive v3/v4 messages without changing any
//! existing payload: [`ServerMessage::LightUpdate`] carries compressed dense
//! sky+RGB words for the sections a relight changed (see [`LightSectionDto`]
//! and [`encode_light_words`]); [`ServerMessage::TimeSync`] publishes the
//! authoritative world clock; and [`ServerMessage::SleepState`] plus
//! [`ClientMessage::SleepRequest`]/[`ClientMessage::Wake`] drive bed sleep and
//! the 100-tick sunrise fast-forward. Clients that do not understand a new
//! variant must ignore it (the interim client already has a catch-all arm).
//!
//! # Chunk snapshots
//!
//! [`encode_chunk`] writes a deterministic paletted column with per-section
//! storage modes, compact light and optional zstd at level 1. [`decode_chunk`]
//! validates the payload against a [`ob_content::Registry`] so a mismatched
//! content pack is rejected instead of silently misinterpreted. Chunk bytes
//! travel inside JSON as base64 via [`ServerMessage::ChunkData`].
//!
//! # Example
//!
//! ```
//! use ob_protocol::{ClientMessage, PROTOCOL_VERSION, encode_client_frame, decode_client_frame};
//!
//! let message = ClientMessage::Hello {
//!     protocol: PROTOCOL_VERSION,
//!     name: "alice".to_string(),
//! };
//! let frame = encode_client_frame(&message).expect("serialisable message");
//! let decoded = decode_client_frame(&frame, ob_protocol::MAX_FRAME_LEN).expect("valid frame");
//! assert_eq!(decoded, message);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod base64;
pub mod chunk;
pub mod frame;
pub mod light;
pub mod limits;
pub mod message;

pub use chunk::{MAX_CHUNK_BODY_BYTES, MAX_CHUNK_BYTES, decode_chunk, encode_chunk, encode_chunk_checked};
pub use frame::{
    MAX_FRAME_LEN, ProtocolError, decode_client_frame, decode_frame, decode_payload, decode_server_frame,
    encode_client_frame, encode_frame, encode_frame_with_limit, encode_server_frame, read_frame, read_message,
    write_frame, write_message, write_message_with_limit,
};
pub use light::{
    LIGHT_CELLS, LIGHT_ENCODING_RAW, LIGHT_ENCODING_ZSTD, LIGHT_SECTION_FORMAT, LIGHT_WORDS_BYTES,
    MAX_LIGHT_SECTION_BYTES, decode_light_words, encode_light_words,
};
pub use limits::RateLimit;
pub use message::{
    BlockDescriptor, CHEST_WINDOW_SLOTS, ClientMessage, ContainerKind, DAY_LENGTH_TICKS, DespawnReason,
    EQUIPMENT_SLOT_START, EQUIPMENT_SLOTS, EntityKind, EntitySnapshot, EntityStateEntry, FURNACE_WINDOW_SLOTS,
    FurnaceProgress, HOTBAR_SLOTS, INVENTORY_SLOTS, ItemStackDto, LightSectionDto, MAIN_SLOT_START, MAIN_SLOTS,
    MAX_CHAT_LEN, MAX_LIGHT_SECTIONS_PER_UPDATE, MAX_MOTD_LEN, MAX_NAME_LEN, PROTOCOL_VERSION, PlayerSnapshot,
    SLOT_WINDOW_START, ServerMessage, block_manifest, content_fingerprint, default_max_frame_len, manifest_matches,
    sanitise_motd, validate_chat_text, validate_player_name,
};

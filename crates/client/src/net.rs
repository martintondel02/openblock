//! Headless client-side multiplayer core for the interim JSON/TCP protocol.
//!
//! This module is deliberately independent of winit and the renderer so it can
//! be exercised by loopback integration tests on CI hosts without a GPU. It
//! owns the TCP connection, the replicated [`World`], the local [`Player`], the
//! remote-player table, chat, chunk request bookkeeping and the protocol v3
//! survival state ([`SurvivalState`]), replicated mobs/items
//! ([`ReplicatedEntities`]) and container windows ([`RemoteContainers`]). The
//! windowed client (`net_app`) and the headless tests share exactly this API.
//!
//! # Threading
//!
//! [`NetworkClient::connect`] performs the blocking Hello handshake (bounded by
//! [`CONNECT_TIMEOUT`]) and then spawns two threads:
//!
//! * a reader thread that decodes frames and chunk snapshots into a bounded
//!   inbound queue; and
//! * a writer thread that drains a bounded outbound queue.
//!
//! No socket I/O happens while the caller holds a game-state borrow, and no
//! lock is shared with the UI thread. [`NetworkClient::poll`] drains the inbound
//! queue without blocking, so the render loop never waits on the network.
//!
//! # Prediction scope
//!
//! Movement is client-authoritative in the interim protocol: [`NetworkClient::tick_player`]
//! steps `ob_sim::step_player` against the replicated world and queues a `Move`
//! when the player or look angles changed. Block edits (including survival
//! break/place) are predicted visually and sent as a `BlockEdit` intent, then
//! reconciled by the server's authoritative broadcast. A predicted edit that is
//! not confirmed within [`PREDICTION_TIMEOUT`] is reverted, and a
//! `BlockEdit` `ActionRejected` reverts the newest pending prediction
//! immediately. Inventory counts, durability, vitals, drops and container
//! contents are **never** predicted: they only come from the server's sync
//! messages. Movement has no reconciliation in this milestone.
//!
//! # Survival and entity authority
//!
//! [`NetworkClient`] sends [`ClientMessage::SelectSlot`], `UseBlock`, `Attack`,
//! `InventoryMove`, `Eat`, `Equip`, `Respawn` and `ContainerClose` intents and
//! mirrors the replies through the reducers in [`crate::net_state`]. Entity ids
//! are opaque (item ids carry the server's high bit); mob and item positions are
//! interpolated over [`INTERPOLATION_SECONDS`] and cleaned up on despawn. A
//! disconnect clears all session state before the UI can render it.
//!
//! # Disconnect and shutdown
//!
//! Dropping a client (or calling [`NetworkClient::shutdown`]) queues a
//! `Disconnect`, joins the writer so the frame is flushed, interrupts the
//! reader and joins it. A server `Disconnect` or a socket failure transitions
//! [`NetState`] to [`NetState::Disconnected`] with a human-readable reason;
//! every send method then becomes a no-op instead of panicking.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ob_content::{BlockId, Registry};
use ob_math::{Aabb, BlockPos, ChunkPos, N_SECTIONS, SectionPos, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec};
use ob_protocol::{
    ClientMessage, MAX_CHAT_LEN, MAX_FRAME_LEN, PROTOCOL_VERSION, PlayerSnapshot, ProtocolError, RateLimit,
    ServerMessage, content_fingerprint, decode_chunk, decode_payload, encode_client_frame, manifest_matches,
    validate_chat_text, validate_player_name, write_message,
};
use ob_render::EntityInstance;
use ob_server::{ItemStack, MOB_ATTACK_REACH};
use ob_sim::{Player, PlayerInput, REACH, StepEvents, TICK_DT};
use ob_world::{ChunkColumn, World};

use crate::containers::{ClickButton, ContainerSnapshot, SlotId};
use crate::net_state::{
    CursorStack, RemoteClickOutcome, RemoteContainers, ReplicatedEntities, SurvivalState, friendly_action_rejection,
};

/// Default TCP connect and handshake timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest client render/interest radius supported by the interim server.
///
/// 32 chunks is the 512-block playable radius; the server accepts the same
/// ceiling. Clients never request more than this even if a settings file was
/// written by a future build.
pub const MAX_RENDER_RADIUS: i32 = 32;
/// Duration of one remote-player interpolation segment.
pub const INTERPOLATION_SECONDS: f32 = 0.1;
/// How long a chunk request stays deduplicated before it may be re-sent.
pub const CHUNK_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a locally predicted block edit waits for server confirmation.
pub const PREDICTION_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum number of chat lines retained by the overlay.
pub const MAX_CHAT_LINES: usize = 100;
/// Maximum number of characters retained for one chat sender.
pub const MAX_CHAT_SENDER_CHARS: usize = 32;
/// Maximum number of characters retained for one chat line.
///
/// This mirrors the protocol's [`MAX_CHAT_LEN`] for client-originated text; a
/// hostile or buggy server can send more, so the log truncates defensively.
pub const MAX_CHAT_TEXT_CHARS: usize = 256;
/// Maximum UTF-8 payload bytes retained by the whole chat log.
///
/// The line and per-field caps already bound a full log to roughly 115 KiB;
/// this tighter budget keeps the overlay's memory proportional even when every
/// line uses four-byte scalars.
pub const MAX_CHAT_BYTES: usize = 64 * 1024;
/// Maximum number of remote players tracked from a roster or join/state stream.
pub const MAX_PLAYER_LIST: usize = 1024;
/// Seconds before an old chat line starts fading.
pub const CHAT_FADE_START: f32 = 10.0;
/// Seconds the chat fade lasts.
pub const CHAT_FADE_SECONDS: f32 = 3.0;
/// Maximum number of pending `BlockEdit` intents buffered for missing chunks.
const MAX_PENDING_EDITS: usize = 4096;
/// Maximum number of unconfirmed local block predictions tracked at once.
const MAX_PREDICTIONS: usize = 256;
/// Bounded frame queues (the server's per-client limiter is similar).
const INBOUND_QUEUE: usize = 512;
const OUTBOUND_QUEUE: usize = 256;
/// Reader poll interval: short enough that clean shutdown joins promptly.
const READ_POLL_TIMEOUT: Duration = Duration::from_millis(250);
/// Socket write timeout for the writer thread.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Ping cadence and the age after which an unanswered ping is forgotten.
const PING_INTERVAL: Duration = Duration::from_secs(1);
const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// Disconnect after this long without any server frame. The server sends
/// player states at 10 Hz, so silence means the connection is black-holed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Options for one connection attempt.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// `HOST:PORT` endpoint.
    pub endpoint: String,
    /// Requested player name (`[A-Za-z0-9_.-]{1,32}`).
    pub name: String,
    /// Desired chunk radius around the player, clamped to `1..=32`.
    pub render_radius: i32,
    /// Local content registry; the handshake must match its manifest.
    pub registry: Arc<Registry>,
    /// TCP connect and handshake timeout.
    pub connect_timeout: Duration,
}

impl ConnectOptions {
    /// Creates options with [`CONNECT_TIMEOUT`] and a clamped radius.
    pub fn new(
        endpoint: impl Into<String>,
        name: impl Into<String>,
        render_radius: i32,
        registry: Arc<Registry>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            name: name.into(),
            render_radius: render_radius.clamp(1, MAX_RENDER_RADIUS),
            registry,
            connect_timeout: CONNECT_TIMEOUT,
        }
    }
}

/// A connection or protocol failure with a user-facing description.
#[derive(Debug)]
pub enum NetError {
    /// The endpoint is not a resolvable `HOST:PORT` address.
    InvalidEndpoint(String),
    /// Socket I/O failed.
    Io(io::Error),
    /// Connecting or the handshake exceeded its deadline.
    Timeout(String),
    /// A frame could not be framed or decoded.
    Protocol(ProtocolError),
    /// The server explicitly refused the connection (`Disconnect { reason }`).
    Refused(String),
    /// The server speaks a different protocol version.
    ProtocolMismatch {
        /// Version advertised by the server.
        server: u16,
        /// Version implemented locally.
        client: u16,
    },
    /// The server's content fingerprint or block manifest does not match.
    ContentMismatch {
        /// Fingerprint advertised by the server.
        server: u64,
        /// Fingerprint computed from the local registry.
        client: u64,
        /// Whether the manifest itself differed (not only the fingerprint).
        manifest: bool,
    },
    /// Another handshake invariant failed (ordering, missing roster entry, ...).
    Handshake(String),
}

impl fmt::Display for NetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::InvalidEndpoint(reason) => write!(formatter, "invalid server address: {reason}"),
            NetError::Io(error) => write!(formatter, "network i/o error: {error}"),
            NetError::Timeout(reason) => write!(formatter, "network timeout: {reason}"),
            NetError::Protocol(error) => write!(formatter, "protocol error: {error}"),
            NetError::Refused(reason) => write!(formatter, "server refused the connection: {reason}"),
            NetError::ProtocolMismatch { server, client } => write!(
                formatter,
                "protocol mismatch: server speaks v{server}, this client speaks v{client}; both sides must be a 1.2+ build (protocol v3)"
            ),
            NetError::ContentMismatch {
                server,
                client,
                manifest,
            } => {
                if *manifest {
                    write!(
                        formatter,
                        "content mismatch: server block manifest differs (fingerprint {server:016x}, local {client:016x})"
                    )
                } else {
                    write!(
                        formatter,
                        "content mismatch: server fingerprint {server:016x}, local {client:016x}"
                    )
                }
            }
            NetError::Handshake(reason) => write!(formatter, "handshake failed: {reason}"),
        }
    }
}

impl std::error::Error for NetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NetError::Io(error) => Some(error),
            NetError::Protocol(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for NetError {
    fn from(error: io::Error) -> Self {
        NetError::Io(error)
    }
}

impl From<ProtocolError> for NetError {
    fn from(error: ProtocolError) -> Self {
        NetError::Protocol(error)
    }
}

/// Lifecycle of an established client after the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetState {
    /// Handshake finished but the spawn chunk and neighbours are still streaming.
    Loading,
    /// Enough of the spawn area is resident to simulate and render.
    Playing,
    /// The connection is gone; sends are disabled.
    Disconnected(String),
}

impl NetState {
    /// Whether the connection can still send and receive.
    pub fn is_connected(&self) -> bool {
        !matches!(self, NetState::Disconnected(_))
    }

    /// Whether the loading threshold has been reached.
    pub fn is_playing(&self) -> bool {
        matches!(self, NetState::Playing)
    }

    /// Transitions to disconnected; returns `true` only for the first call.
    pub fn disconnect(&mut self, reason: impl Into<String>) -> bool {
        if self.is_connected() {
            *self = NetState::Disconnected(reason.into());
            true
        } else {
            false
        }
    }

    /// The disconnect reason, if any.
    pub fn disconnect_reason(&self) -> Option<&str> {
        match self {
            NetState::Disconnected(reason) => Some(reason),
            _ => None,
        }
    }
}

/// Metadata from the server's handshake `Hello`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    /// Human-readable world name.
    pub world_name: String,
    /// World seed.
    pub seed: u64,
    /// Recommended spawn position (feet).
    pub spawn: WorldVec,
    /// Server content fingerprint.
    pub content_fingerprint: u64,
    /// Server tick at join.
    pub tick: u64,
    /// Ticks per in-game day.
    pub day_ticks: u64,
    /// Configured player cap.
    pub max_players: u16,
    /// Server message of the day.
    pub motd: String,
}

/// One line in the chat log.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatLine {
    /// Display name of the sender (`server` for console lines).
    pub from: String,
    /// Plain text.
    pub text: String,
    /// Seconds since the line arrived.
    pub age: f32,
}

impl ChatLine {
    /// Creates a fresh line.
    pub fn new(from: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            text: text.into(),
            age: 0.0,
        }
    }

    /// Opacity for the overlay: full for [`CHAT_FADE_START`] seconds, then a
    /// linear fade over [`CHAT_FADE_SECONDS`].
    pub fn alpha(&self) -> f32 {
        if !self.age.is_finite() || self.age <= CHAT_FADE_START {
            return 1.0;
        }
        let faded = (self.age - CHAT_FADE_START) / CHAT_FADE_SECONDS;
        (1.0 - faded).clamp(0.0, 1.0)
    }
}

/// Bounded chat history with wall-clock-free fading.
#[derive(Debug, Clone, Default)]
pub struct ChatLog {
    lines: VecDeque<ChatLine>,
    bytes: usize,
}

impl ChatLog {
    /// Appends a line after sanitising and bounding it.
    ///
    /// Sender and text are truncated on character (not byte) boundaries, and
    /// every control character (including `\n`, `\r` and `\t`) becomes a space
    /// so a server-supplied line cannot disturb the single-line overlay. The
    /// oldest lines are evicted until the new line fits both
    /// [`MAX_CHAT_LINES`] and [`MAX_CHAT_BYTES`].
    pub fn push(&mut self, from: impl Into<String>, text: impl Into<String>) {
        let from = sanitise_chat_component(from.into(), MAX_CHAT_SENDER_CHARS);
        let text = sanitise_chat_component(text.into(), MAX_CHAT_TEXT_CHARS);
        let line_bytes = from.len().saturating_add(text.len());
        // Unreachable with the current caps, but keeps a caller-supplied
        // constant change from letting one line exceed the log budget.
        if line_bytes > MAX_CHAT_BYTES {
            return;
        }
        while !self.lines.is_empty()
            && (self.lines.len() >= MAX_CHAT_LINES || self.bytes.saturating_add(line_bytes) > MAX_CHAT_BYTES)
        {
            if let Some(oldest) = self.lines.pop_front() {
                self.bytes = self
                    .bytes
                    .saturating_sub(oldest.from.len().saturating_add(oldest.text.len()));
            }
        }
        self.bytes = self.bytes.saturating_add(line_bytes);
        self.lines.push_back(ChatLine::new(from, text));
    }

    /// Advances every line's age.
    pub fn tick(&mut self, dt_seconds: f32) {
        if !dt_seconds.is_finite() || dt_seconds <= 0.0 {
            return;
        }
        for line in &mut self.lines {
            line.age += dt_seconds;
        }
    }

    /// All retained lines, oldest first.
    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &ChatLine> + '_ {
        self.lines.iter()
    }

    /// The newest `max` lines that have not completely faded, oldest first.
    pub fn visible(&self, max: usize) -> Vec<&ChatLine> {
        let mut visible: Vec<&ChatLine> = self
            .lines
            .iter()
            .rev()
            .filter(|line| line.alpha() > 0.001)
            .take(max)
            .collect();
        visible.reverse();
        visible
    }

    /// Number of retained lines.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Retained sender/text UTF-8 bytes across every line.
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Removes every line.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
    }
}

/// Replaces control characters with spaces and truncates on a character
/// boundary, appending `...` when anything was removed.
fn sanitise_chat_component(value: String, max_chars: usize) -> String {
    let mut sanitised = String::new();
    let mut truncated = false;
    for (chars, character) in value.chars().enumerate() {
        if chars == max_chars {
            truncated = true;
            break;
        }
        sanitised.push(if character.is_control() { ' ' } else { character });
    }
    if truncated && max_chars >= 3 {
        let keep = max_chars - 3;
        let mut shortened: String = sanitised.chars().take(keep).collect();
        shortened.push_str("...");
        sanitised = shortened;
    }
    sanitised
}

/// Text input for the chat overlay.
///
/// Only printable ASCII is accepted, matching the interim protocol's practical
/// subset and keeping the bitmap font legible. The input owns its buffer so the
/// UI and its tests do not need winit.
#[derive(Debug, Clone, Default)]
pub struct ChatInput {
    text: Option<String>,
}

impl ChatInput {
    /// Opens an empty input.
    pub fn open(&mut self) {
        self.text.get_or_insert_with(String::new);
    }

    /// Closes the input and discards its text.
    pub fn cancel(&mut self) {
        self.text = None;
    }

    /// Whether the input is accepting keys.
    pub fn is_active(&self) -> bool {
        self.text.is_some()
    }

    /// The current buffer, if active.
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Appends one printable ASCII character.
    ///
    /// Returns `false` for control/non-ASCII characters and once the protocol
    /// limit is reached.
    pub fn push_char(&mut self, character: char) -> bool {
        let Some(text) = self.text.as_mut() else {
            return false;
        };
        if !(character.is_ascii_graphic() || character == ' ') || text.len() >= MAX_CHAT_LEN {
            return false;
        }
        text.push(character);
        true
    }

    /// Removes the last character.
    pub fn backspace(&mut self) -> bool {
        self.text.as_mut().is_some_and(|text| text.pop().is_some())
    }

    /// Closes the input and returns the trimmed, non-empty line, if any.
    pub fn submit(&mut self) -> Option<String> {
        let text = self.text.take()?;
        let text = text.trim();
        if text.is_empty() { None } else { Some(text.to_string()) }
    }
}

/// One player in the Tab overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerEntry {
    /// Connection id (unique for the server process lifetime).
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Whether this is the local client.
    pub is_self: bool,
}

/// A remote player's last two snapshots with interpolation state.
#[derive(Debug, Clone)]
pub struct RemotePlayer {
    /// Connection id.
    pub id: u64,
    /// Display name.
    pub name: String,
    from: [f64; 3],
    to: [f64; 3],
    yaw_from: f32,
    yaw_to: f32,
    pitch: f32,
    updated: Instant,
}

impl RemotePlayer {
    /// Creates a player anchored at one snapshot.
    pub fn new(snapshot: &PlayerSnapshot, now: Instant) -> Self {
        Self {
            id: snapshot.id,
            name: snapshot.name.clone(),
            from: snapshot.pos,
            to: snapshot.pos,
            yaw_from: snapshot.yaw,
            yaw_to: snapshot.yaw,
            pitch: snapshot.pitch,
            updated: now,
        }
    }

    /// Pushes a newer snapshot, making the previous target the interpolation start.
    pub fn update(&mut self, pos: [f64; 3], yaw: f32, pitch: f32, now: Instant) {
        if !pos.iter().all(|value| value.is_finite()) || !yaw.is_finite() || !pitch.is_finite() {
            return;
        }
        self.from = self.to;
        self.yaw_from = self.yaw_to;
        self.to = pos;
        self.yaw_to = yaw;
        self.pitch = pitch;
        self.updated = now;
    }

    /// Interpolated render position and angles at `now`.
    ///
    /// The result lags the server by one snapshot interval
    /// ([`INTERPOLATION_SECONDS`]), which is what makes 10 Hz snapshots look
    /// continuous.
    pub fn interpolated(&self, now: Instant) -> ([f64; 3], f32, f32) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f32();
        let t = (elapsed / INTERPOLATION_SECONDS).clamp(0.0, 1.0);
        let position = [
            self.from[0] + (self.to[0] - self.from[0]) * f64::from(t),
            self.from[1] + (self.to[1] - self.from[1]) * f64::from(t),
            self.from[2] + (self.to[2] - self.from[2]) * f64::from(t),
        ];
        let yaw = lerp_angle(self.yaw_from, self.yaw_to, t);
        let pitch = self.pitch;
        (position, yaw, pitch)
    }
}

/// One remote player ready for avatar instance generation.
#[derive(Debug, Clone, PartialEq)]
pub struct AvatarState {
    /// Connection id (used with the name for the stable colour hash).
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Interpolated feet position.
    pub pos: [f64; 3],
    /// Interpolated yaw in radians.
    pub yaw: f32,
}

/// Number of cubes in one blocky avatar (two legs, torso and head).
pub const AVATAR_CUBES: usize = 4;

/// Stable per-player avatar colour derived from id and name.
///
/// FNV-1a over the id and UTF-8 name selects a saturated hue, so the same
/// player keeps the same colour across sessions and reconnects.
pub fn avatar_color(id: u64, name: &str) -> [f32; 3] {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in id.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let hue = (hash % 3600) as f32 / 3600.0;
    hsv_to_rgb(hue, 0.58, 0.94)
}

/// Builds four flat cubes per remote player: two legs, torso and head.
///
/// The avatar is intentionally stylised because [`EntityInstance`] can only
/// draw uniform cubes. Legs and head are offset along the player's facing
/// direction so yaw is visible even with flat colours.
pub fn avatar_instances(avatars: &[AvatarState]) -> Vec<EntityInstance> {
    let mut instances = Vec::with_capacity(avatars.len() * AVATAR_CUBES);
    for avatar in avatars {
        let [x, y, z] = avatar.pos;
        let yaw = avatar.yaw;
        let (sin_yaw, cos_yaw) = f64::from(yaw).sin_cos();
        let right = [cos_yaw, 0.0, -sin_yaw];
        let forward = [-sin_yaw, 0.0, -cos_yaw];
        let base = avatar_color(avatar.id, &avatar.name);
        let head_tint = [base[0], base[1], base[2]];
        let torso_tint = [base[0] * 0.86, base[1] * 0.86, base[2] * 0.86];
        let leg_tint = [base[0] * 0.64, base[1] * 0.64, base[2] * 0.64];

        // Head: 0.5 m cube whose base sits on the torso at 1.0 m, nudged
        // forward slightly so the yaw orientation is readable.
        instances.push(EntityInstance {
            position: [
                (x + forward[0] * 0.06) as f32,
                (y + 1.0) as f32,
                (z + forward[2] * 0.06) as f32,
            ],
            scale: 0.5,
            yaw,
            layer: 0,
            tint: head_tint,
            textured: false,
        });
        // Torso: 0.6 m cube from 0.4 m to 1.0 m.
        instances.push(EntityInstance {
            position: [x as f32, (y + 0.4) as f32, z as f32],
            scale: 0.6,
            yaw,
            layer: 0,
            tint: torso_tint,
            textured: false,
        });
        // Two 0.4 m legs with a small gap; they rotate around the body.
        for side in [-1.0, 1.0] {
            instances.push(EntityInstance {
                position: [
                    (x + right[0] * 0.15 * side) as f32,
                    y as f32,
                    (z + right[2] * 0.15 * side) as f32,
                ],
                scale: 0.4,
                yaw,
                layer: 0,
                tint: leg_tint,
                textured: false,
            });
        }
    }
    instances
}

/// Converts a hue/saturation/value triple to linear-ish sRGB components.
fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> [f32; 3] {
    let h = (hue.fract() + 1.0).fract() * 6.0;
    let sector = h.floor();
    let fraction = h - sector;
    let p = value * (1.0 - saturation);
    let q = value * (1.0 - saturation * fraction);
    let t = value * (1.0 - saturation * (1.0 - fraction));
    match sector as i32 % 6 {
        0 => [value, t, p],
        1 => [q, value, p],
        2 => [p, value, t],
        3 => [p, q, value],
        4 => [t, p, value],
        _ => [value, p, q],
    }
}

/// Interpolates two angles along the shortest arc.
fn lerp_angle(from: f32, to: f32, t: f32) -> f32 {
    let mut delta = (to - from) % std::f32::consts::TAU;
    if delta > std::f32::consts::PI {
        delta -= std::f32::consts::TAU;
    } else if delta < -std::f32::consts::PI {
        delta += std::f32::consts::TAU;
    }
    from + delta * t
}

/// Tracks the desired chunk ring, what is loaded and which requests are in flight.
///
/// This is pure bookkeeping (no world access) so request de-duplication,
/// timeout re-requests and the loading threshold are unit-testable without a
/// server.
#[derive(Debug, Clone)]
pub struct ChunkTracker {
    radius: i32,
    center: ChunkPos,
    spawn: ChunkPos,
    target: Vec<ChunkPos>,
    loaded: HashSet<ChunkPos>,
    requested: HashMap<ChunkPos, Instant>,
}

impl ChunkTracker {
    /// Creates a tracker centred on `spawn` with a radius clamped to `1..=32`.
    pub fn new(radius: i32, spawn: ChunkPos) -> Self {
        let radius = radius.clamp(1, MAX_RENDER_RADIUS);
        let target = ring(radius, spawn);
        Self {
            radius,
            center: spawn,
            spawn,
            target,
            loaded: HashSet::new(),
            requested: HashMap::new(),
        }
    }

    /// Current radius in chunks.
    pub fn radius(&self) -> i32 {
        self.radius
    }

    /// Changes the radius, rebuilding the target ring.
    pub fn set_radius(&mut self, radius: i32) {
        let radius = radius.clamp(1, MAX_RENDER_RADIUS);
        if radius != self.radius {
            self.radius = radius;
            self.target = ring(radius, self.center);
        }
    }

    /// Current centre chunk.
    pub fn center(&self) -> ChunkPos {
        self.center
    }

    /// Recentres the desired ring; loaded chunks outside it are retained.
    pub fn set_center(&mut self, center: ChunkPos) {
        if center != self.center {
            self.center = center;
            self.target = ring(self.radius, center);
        }
    }

    /// Marks a column resident.
    pub fn mark_loaded(&mut self, pos: ChunkPos) {
        self.loaded.insert(pos);
        self.requested.remove(&pos);
    }

    /// Marks a column as no longer resident after local eviction.
    ///
    /// The column leaves the `loaded` set so the next ring request can fetch it
    /// again, and any outstanding request token is forgotten.
    pub fn mark_unloaded(&mut self, pos: ChunkPos) {
        self.loaded.remove(&pos);
        self.requested.remove(&pos);
    }

    /// Records that a request was actually queued for `pos`.
    pub fn mark_requested(&mut self, pos: ChunkPos, now: Instant) {
        self.requested.insert(pos, now);
    }

    /// Whether a column is loaded.
    pub fn is_loaded(&self, pos: ChunkPos) -> bool {
        self.loaded.contains(&pos)
    }

    /// Number of columns in the desired ring.
    pub fn target_count(&self) -> usize {
        self.target.len()
    }

    /// Number of desired columns that are resident.
    pub fn loaded_count(&self) -> usize {
        self.target.iter().filter(|pos| self.loaded.contains(pos)).count()
    }

    /// `(loaded, target)` for the loading overlay.
    pub fn progress(&self) -> (usize, usize) {
        (self.loaded_count(), self.target_count())
    }

    /// Number of requests still awaiting a reply.
    pub fn pending_count(&self) -> usize {
        self.requested.len()
    }

    /// Whether the spawn chunk and its four cardinal neighbours are resident.
    pub fn is_ready(&self) -> bool {
        if self.target.iter().all(|pos| self.loaded.contains(pos)) {
            return true;
        }
        let required = [
            self.spawn,
            ChunkPos::new(self.spawn.x + 1, self.spawn.z),
            ChunkPos::new(self.spawn.x - 1, self.spawn.z),
            ChunkPos::new(self.spawn.x, self.spawn.z + 1),
            ChunkPos::new(self.spawn.x, self.spawn.z - 1),
        ];
        required.iter().all(|pos| self.loaded.contains(pos))
    }

    /// Returns up to `budget` deterministic missing-column requests.
    ///
    /// Requests older than [`CHUNK_REQUEST_TIMEOUT`] are forgotten so a lost
    /// request can be retried. Call [`Self::mark_requested`] after the frame was
    /// queued successfully; merely reading this list does not reserve anything.
    pub fn next_requests(&mut self, budget: usize, now: Instant) -> Vec<ChunkPos> {
        if budget == 0 {
            return Vec::new();
        }
        self.requested
            .retain(|_, requested| now.saturating_duration_since(*requested) < CHUNK_REQUEST_TIMEOUT);
        let mut requests = Vec::new();
        for position in &self.target {
            if self.loaded.contains(position) || self.requested.contains_key(position) {
                continue;
            }
            requests.push(*position);
            if requests.len() >= budget {
                break;
            }
        }
        requests
    }
}

/// Chunk positions in the same deterministic square-ring order the server uses.
fn ring(radius: i32, center: ChunkPos) -> Vec<ChunkPos> {
    let mut positions = Vec::with_capacity(((radius * 2 + 1) * (radius * 2 + 1)) as usize);
    for dz in -radius..=radius {
        for dx in -radius..=radius {
            positions.push(ChunkPos::new(center.x + dx, center.z + dz));
        }
    }
    positions
}

/// Whether a player snapshot's position and angles are finite.
fn finite_snapshot(pos: [f64; 3], yaw: f32, pitch: f32) -> bool {
    pos.iter().all(|value| value.is_finite()) && yaw.is_finite() && pitch.is_finite()
}

/// Horizontal world-border limit enforced by the server (`±30,000,000`).
const WORLD_BORDER_XZ: f64 = 30_000_000.0;
/// Lowest feet Y accepted from the wire (`WORLD_MIN_Y - 32`).
const PLAYER_MIN_FEET_Y: f64 = (WORLD_MIN_Y - 32) as f64;
/// Highest feet Y accepted from the wire (`WORLD_MAX_Y + 256`).
const PLAYER_MAX_FEET_Y: f64 = (WORLD_MAX_Y + 256) as f64;

/// Sanitises a server-provided player position.
///
/// Returns `None` for a non-finite component (NaN/±Inf) and otherwise clamps
/// X/Z to the world border and Y to the same vertical allowance the server
/// applies to movement. Never returns an unbounded or non-finite vector.
fn sanitise_player_pos(pos: [f64; 3]) -> Option<WorldVec> {
    if !pos.iter().all(|value| value.is_finite()) {
        return None;
    }
    Some(WorldVec::new(
        pos[0].clamp(-WORLD_BORDER_XZ, WORLD_BORDER_XZ),
        pos[1].clamp(PLAYER_MIN_FEET_Y, PLAYER_MAX_FEET_Y),
        pos[2].clamp(-WORLD_BORDER_XZ, WORLD_BORDER_XZ),
    ))
}

/// Resolves a `respawn` position, falling back to the last known good/local
/// position when the wire value is non-finite.
fn resolve_respawn_pos(pos: [f64; 3], fallback: WorldVec) -> WorldVec {
    sanitise_player_pos(pos)
        .or_else(|| sanitise_player_pos([fallback.x, fallback.y, fallback.z]))
        .unwrap_or_else(|| WorldVec::new(0.5, (WORLD_MIN_Y + 1) as f64, 0.5))
}

/// Bounds the roster consumed by the blocking handshake.
///
/// A hostile server can send a very large `player_list` (still inside the
/// frame cap). Keep at most [`MAX_PLAYER_LIST`] entries, but retain this
/// client's own entry when it appears beyond the cut so the handshake can
/// still identify the local player.
fn cap_handshake_roster(
    mut roster: Vec<PlayerSnapshot>,
    local_id: Option<u64>,
    requested_name: &str,
) -> Vec<PlayerSnapshot> {
    if roster.len() <= MAX_PLAYER_LIST {
        return roster;
    }
    let self_entry = roster
        .iter()
        .position(|player| {
            local_id == Some(player.id) || (local_id.is_none() && player.name.eq_ignore_ascii_case(requested_name))
        })
        .map(|index| roster[index].clone());
    roster.truncate(MAX_PLAYER_LIST);
    if let Some(entry) = self_entry
        && !roster.iter().any(|player| player.id == entry.id)
    {
        roster.pop();
        roster.push(entry);
    }
    roster
}

/// Whether the 3x3 chunk area around a feet position is resident.
///
/// Collision reads blocks from the current column and its neighbours, so the
/// local player is only stepped when that small area is fully available.
fn terrain_ready(world: &World, position: WorldVec) -> bool {
    let center = chunk_of(position);
    for dz in -1..=1 {
        for dx in -1..=1 {
            if world.chunk(ChunkPos::new(center.x + dx, center.z + dz)).is_none() {
                return false;
            }
        }
    }
    true
}

/// Chunk containing a feet position.
fn chunk_of(position: WorldVec) -> ChunkPos {
    BlockPos::new(
        position.x.floor() as i32,
        position.y.floor() as i32,
        position.z.floor() as i32,
    )
    .chunk()
}

/// Events drained by the UI frame.
#[derive(Debug, Clone, PartialEq)]
pub enum NetEvent {
    /// A remote player joined.
    Joined {
        /// Connection id.
        id: u64,
        /// Display name.
        name: String,
    },
    /// A remote player left.
    Left {
        /// Connection id.
        id: u64,
        /// Display name.
        name: String,
    },
    /// A chat line arrived (also appended to [`NetworkClient::chat_log`]).
    Chat {
        /// Sender.
        from: String,
        /// Text.
        text: String,
    },
    /// The local player took damage.
    Damage {
        /// Damage removed after armor.
        amount: f32,
        /// Short cause (`fall`, `starvation`, `mob`, ...).
        cause: String,
    },
    /// The local player died.
    Death {
        /// Short cause.
        cause: String,
    },
    /// The local player respawned at the server spawn.
    Respawned {
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
    },
    /// A gameplay action was rejected by the server.
    ActionRejected {
        /// Short action name (`block_edit`, `inventory_move`, ...).
        action: String,
        /// Short machine-readable reason.
        reason: String,
        /// Human-readable hint for a toast.
        hint: String,
    },
    /// A server container window opened.
    ContainerOpened {
        /// Window id.
        window: u32,
    },
    /// The server container window closed.
    ContainerClosed {
        /// Window id.
        window: u32,
        /// Machine-readable reason.
        reason: String,
    },
    /// The connection ended; this is emitted once.
    Disconnected {
        /// Human-readable reason.
        reason: String,
    },
    /// The local cursor preview no longer fits the authoritative container.
    ContainerCursorDropped,
}

/// One item produced by the reader thread.
enum Inbound {
    Message(Box<ServerMessage>),
    Chunk(Box<ChunkColumn>),
    Closed(String),
}

/// A locally predicted block edit awaiting server confirmation.
#[derive(Debug, Clone, Copy)]
struct Prediction {
    previous: BlockId,
    placed: BlockId,
    at: Instant,
}

/// Bounded insertion-ordered set of unconfirmed block predictions.
///
/// The order makes "revert the newest prediction" deterministic when the
/// server rejects a `block_edit` without naming the affected cell. The map
/// preserves the original pre-prediction block for every cell, so repeated
/// edits of the same cell before a broadcast still revert safely.
#[derive(Debug, Default)]
struct Predictions {
    entries: HashMap<BlockPos, Prediction>,
    order: Vec<BlockPos>,
}

impl Predictions {
    fn remember(&mut self, pos: BlockPos, previous: BlockId, placed: BlockId, now: Instant) {
        let previous = self
            .entries
            .get(&pos)
            .map_or(previous, |prediction| prediction.previous);
        if self.entries.contains_key(&pos) {
            self.order.retain(|remembered| *remembered != pos);
        } else {
            while self.entries.len() >= MAX_PREDICTIONS {
                let Some(oldest) = self.order.first().copied() else {
                    break;
                };
                self.order.remove(0);
                self.entries.remove(&oldest);
            }
        }
        self.order.push(pos);
        self.entries.insert(
            pos,
            Prediction {
                previous,
                placed,
                at: now,
            },
        );
    }

    fn confirm(&mut self, pos: &BlockPos) {
        if self.entries.remove(pos).is_some() {
            self.order.retain(|remembered| remembered != pos);
        }
    }

    /// Removes and returns the newest unconfirmed prediction.
    fn revert_latest(&mut self) -> Option<(BlockPos, Prediction)> {
        while let Some(pos) = self.order.pop() {
            if let Some(prediction) = self.entries.remove(&pos) {
                return Some((pos, prediction));
            }
        }
        None
    }

    /// Removes and returns predictions older than [`PREDICTION_TIMEOUT`].
    fn expire(&mut self, now: Instant) -> Vec<(BlockPos, Prediction)> {
        let expired: Vec<BlockPos> = self
            .entries
            .iter()
            .filter(|(_, prediction)| now.saturating_duration_since(prediction.at) >= PREDICTION_TIMEOUT)
            .map(|(pos, _)| *pos)
            .collect();
        let mut result = Vec::with_capacity(expired.len());
        for pos in expired {
            if let Some(prediction) = self.entries.remove(&pos) {
                result.push((pos, prediction));
            }
        }
        if !result.is_empty() {
            self.order.retain(|pos| self.entries.contains_key(pos));
        }
        result
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

/// A buffered frame reader that survives socket read timeouts.
///
/// `ob_protocol::read_message` calls `read_exact`, so a short read timeout that
/// fires between the header and payload would lose partially consumed bytes and
/// desynchronise the stream. This reader accumulates bytes and only consumes a
/// complete frame, which lets the reader thread poll for shutdown or errors
/// every [`READ_POLL_TIMEOUT`] without corrupting the connection.
struct FrameReader {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl FrameReader {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    /// Reads one complete server frame, returning `WouldBlock`/`TimedOut`
    /// without consuming a partial frame when the socket timeout expires.
    fn read_server_message(&mut self, max_len: usize) -> Result<ServerMessage, ProtocolError> {
        loop {
            if let Some(message) = self.try_decode(max_len)? {
                return Ok(message);
            }
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(ProtocolError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "server closed the connection",
                    )));
                }
                Ok(read) => {
                    self.buffer.extend_from_slice(&chunk[..read]);
                    if self.buffer.len() > max_len.saturating_add(64 * 1024) {
                        return Err(ProtocolError::FrameTooLarge {
                            len: self.buffer.len(),
                            max: max_len,
                        });
                    }
                }
                Err(error) => return Err(ProtocolError::Io(error)),
            }
        }
    }

    /// Decodes and removes one complete frame, if the buffer holds one.
    fn try_decode(&mut self, max_len: usize) -> Result<Option<ServerMessage>, ProtocolError> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let declared = u32::from_be_bytes([self.buffer[0], self.buffer[1], self.buffer[2], self.buffer[3]]) as usize;
        if declared == 0 {
            return Err(ProtocolError::EmptyFrame);
        }
        if declared > max_len {
            return Err(ProtocolError::FrameTooLarge {
                len: declared,
                max: max_len,
            });
        }
        let total = 4 + declared;
        if self.buffer.len() < total {
            return Ok(None);
        }
        let message = decode_payload(&self.buffer[4..total], max_len)?;
        self.buffer.drain(..total);
        Ok(Some(message))
    }
}

/// The connected client: replicated world, local player and network threads.
pub struct NetworkClient {
    registry: Arc<Registry>,
    endpoint: String,
    info: ServerInfo,
    local_id: u64,
    player_name: String,
    world: World,
    player: Player,
    state: NetState,
    tracker: ChunkTracker,
    remote: HashMap<u64, RemotePlayer>,
    chat_log: ChatLog,
    chat_input: ChatInput,
    pending_edits: HashMap<ChunkPos, Vec<(BlockPos, BlockId)>>,
    pending_edit_count: usize,
    predictions: Predictions,
    survival: SurvivalState,
    entities: ReplicatedEntities,
    containers: RemoteContainers,
    /// Blocks further container clicks until the server confirms the last
    /// `inventory_move`, so a high-latency double click cannot duplicate items.
    container_awaiting_sync: bool,
    /// Cursor preview before the last transfer, restored if the server rejects
    /// it (`None` means no rollback is pending).
    container_cursor_rollback: Option<Option<CursorStack>>,
    last_server_state: Option<PlayerSnapshot>,
    ping_ms: Option<u32>,
    pending_ping: Option<(u32, Instant)>,
    next_ping_id: u32,
    last_ping: Instant,
    last_inbound: Instant,
    joined_at: Instant,
    session_ticks: u64,
    last_sent_move: Option<([f64; 3], f32, f32)>,
    move_limit: RateLimit,
    edit_limit: RateLimit,
    chat_limit: RateLimit,
    ping_limit: RateLimit,
    request_limit: RateLimit,
    select_limit: RateLimit,
    use_limit: RateLimit,
    attack_limit: RateLimit,
    inventory_limit: RateLimit,
    eat_limit: RateLimit,
    equip_limit: RateLimit,
    respawn_limit: RateLimit,
    close_limit: RateLimit,
    inbound: Receiver<Inbound>,
    outbound: Option<SyncSender<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    socket: TcpStream,
    shutdown: Arc<AtomicBool>,
}

impl NetworkClient {
    /// Connects, validates the handshake and spawns the reader/writer threads.
    ///
    /// The handshake is bounded by [`ConnectOptions::connect_timeout`] and
    /// returns after the server's `PlayerList`; chunk streaming continues
    /// asynchronously and is drained by [`Self::poll`].
    pub fn connect(options: ConnectOptions) -> Result<Self, NetError> {
        validate_player_name(&options.name)
            .map_err(|error| NetError::Handshake(format!("invalid player name: {error}")))?;
        let timeout = options.connect_timeout.max(Duration::from_millis(100));
        let addresses = resolve_endpoint(&options.endpoint)?;
        let mut stream = None;
        let mut timed_out = false;
        let mut last_error = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, timeout) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => {
                    timed_out |= matches!(error.kind(), io::ErrorKind::TimedOut);
                    last_error = Some(error);
                }
            }
        }
        let stream = match stream {
            Some(stream) => stream,
            None if timed_out => return Err(NetError::Timeout(format!("could not connect to {}", options.endpoint))),
            None => {
                return Err(NetError::Io(last_error.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no address resolved")
                })));
            }
        };
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let requested_name = options.name.clone();

        {
            let mut writer = &stream;
            let hello = ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                name: requested_name.clone(),
            };
            write_message(&mut writer, &hello)?;
            writer.flush()?;
        }

        let deadline = Instant::now() + timeout;
        let mut hello: Option<ServerInfo> = None;
        let mut local_id: Option<u64> = None;
        let mut self_snapshot: Option<PlayerSnapshot> = None;
        let mut frame_reader = FrameReader::new(stream);
        let mut roster: Vec<PlayerSnapshot>;
        loop {
            if Instant::now() >= deadline {
                return Err(NetError::Timeout("handshake timed out".to_string()));
            }
            let message = match frame_reader.read_server_message(MAX_FRAME_LEN) {
                Ok(message) => message,
                Err(ProtocolError::Io(error))
                    if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                {
                    return Err(NetError::Timeout("handshake timed out".to_string()));
                }
                Err(error) => return Err(map_handshake_error(error)),
            };
            match message {
                ServerMessage::Hello { .. } => {
                    if hello.is_some() {
                        return Err(NetError::Handshake("server sent a duplicate hello".to_string()));
                    }
                    hello = Some(accept_hello(&message, &options.registry)?);
                }
                ServerMessage::Join { id, name } => {
                    if hello.is_none() {
                        return Err(NetError::Handshake("server sent join before hello".to_string()));
                    }
                    if local_id.is_none() && name.eq_ignore_ascii_case(&requested_name) {
                        local_id = Some(id);
                    }
                }
                ServerMessage::PlayerState {
                    id,
                    name,
                    pos,
                    yaw,
                    pitch,
                } => {
                    if hello.is_none() {
                        return Err(NetError::Handshake("server sent player_state before hello".to_string()));
                    }
                    if local_id == Some(id) || (local_id.is_none() && name.eq_ignore_ascii_case(&requested_name)) {
                        local_id = Some(id);
                        self_snapshot = Some(PlayerSnapshot {
                            id,
                            name,
                            pos,
                            yaw,
                            pitch,
                        });
                    }
                }
                ServerMessage::PlayerList { players } => {
                    if hello.is_none() {
                        return Err(NetError::Handshake("server sent player_list before hello".to_string()));
                    }
                    roster = players;
                    break;
                }
                ServerMessage::Disconnect { reason } => return Err(NetError::Refused(reason)),
                ServerMessage::ChunkData { .. } => {}
                _ => {}
            }
        }

        let info = hello.ok_or_else(|| NetError::Handshake("server sent player_list before hello".to_string()))?;
        roster = cap_handshake_roster(roster, local_id, &requested_name);
        let self_index = roster
            .iter()
            .position(|player| local_id == Some(player.id))
            .or_else(|| {
                roster
                    .iter()
                    .position(|player| player.name.eq_ignore_ascii_case(&requested_name))
            })
            .ok_or_else(|| NetError::Handshake("server player list did not include this client".to_string()))?;
        let self_snapshot = self_snapshot.unwrap_or_else(|| roster[self_index].clone());
        let local_id = self_snapshot.id;
        let Some(spawn) = sanitise_player_pos(self_snapshot.pos) else {
            return Err(NetError::Handshake(
                "server sent a non-finite local player snapshot".to_string(),
            ));
        };
        if !self_snapshot.yaw.is_finite() || !self_snapshot.pitch.is_finite() {
            return Err(NetError::Handshake(
                "server sent a non-finite local player snapshot".to_string(),
            ));
        }
        let mut player = Player::new(spawn);
        player.yaw = self_snapshot.yaw;
        player.pitch = self_snapshot.pitch;

        frame_reader.stream.set_read_timeout(Some(READ_POLL_TIMEOUT))?;
        frame_reader.stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let writer_stream = frame_reader.stream.try_clone()?;
        let socket = frame_reader.stream.try_clone()?;

        let now = Instant::now();
        let mut remote = HashMap::new();
        for snapshot in &roster {
            if snapshot.id == local_id || !finite_snapshot(snapshot.pos, snapshot.yaw, snapshot.pitch) {
                continue;
            }
            let Some(position) = sanitise_player_pos(snapshot.pos) else {
                continue;
            };
            let clamped = PlayerSnapshot {
                pos: [position.x, position.y, position.z],
                ..snapshot.clone()
            };
            remote.insert(clamped.id, RemotePlayer::new(&clamped, now));
        }

        let (outbound_tx, outbound_rx) = mpsc::sync_channel::<Vec<u8>>(OUTBOUND_QUEUE);
        let (inbound_tx, inbound_rx) = mpsc::sync_channel::<Inbound>(INBOUND_QUEUE);
        let shutdown = Arc::new(AtomicBool::new(false));

        let writer_shutdown = Arc::clone(&shutdown);
        let writer_inbound = inbound_tx.clone();
        let writer = thread::Builder::new()
            .name("ob-client-net-writer".to_string())
            .spawn(move || {
                writer_loop(writer_stream, outbound_rx, writer_inbound, writer_shutdown);
            })
            .map_err(NetError::Io)?;

        let reader_registry = Arc::clone(&options.registry);
        let reader_shutdown = Arc::clone(&shutdown);
        let reader = thread::Builder::new()
            .name("ob-client-net-reader".to_string())
            .spawn(move || {
                reader_loop(frame_reader, reader_registry, inbound_tx, reader_shutdown);
            })
            .map_err(NetError::Io)?;

        let tracker = ChunkTracker::new(options.render_radius, chunk_of(spawn));
        let registry = Arc::clone(&options.registry);
        let world = World::new(options.registry, info.seed);
        let mut client = Self {
            registry,
            endpoint: options.endpoint,
            info,
            local_id,
            player_name: requested_name,
            world,
            player,
            state: NetState::Loading,
            tracker,
            remote,
            chat_log: ChatLog::default(),
            chat_input: ChatInput::default(),
            pending_edits: HashMap::new(),
            pending_edit_count: 0,
            predictions: Predictions::default(),
            survival: SurvivalState::default(),
            entities: ReplicatedEntities::default(),
            containers: RemoteContainers::default(),
            container_awaiting_sync: false,
            container_cursor_rollback: None,
            last_server_state: None,
            ping_ms: None,
            pending_ping: None,
            next_ping_id: 1,
            last_ping: now,
            last_inbound: now,
            joined_at: now,
            session_ticks: 0,
            last_sent_move: None,
            move_limit: RateLimit::new(30.0, 30),
            edit_limit: RateLimit::new(20.0, 20),
            chat_limit: RateLimit::new(0.5, 5),
            ping_limit: RateLimit::new(10.0, 10),
            request_limit: RateLimit::new(128.0, 64),
            select_limit: RateLimit::new(20.0, 20),
            use_limit: RateLimit::new(5.0, 5),
            attack_limit: RateLimit::new(10.0, 10),
            inventory_limit: RateLimit::new(20.0, 30),
            eat_limit: RateLimit::new(4.0, 4),
            equip_limit: RateLimit::new(5.0, 5),
            respawn_limit: RateLimit::new(1.0, 2),
            close_limit: RateLimit::new(10.0, 10),
            inbound: inbound_rx,
            outbound: Some(outbound_tx),
            reader: Some(reader),
            writer: Some(writer),
            socket,
            shutdown,
        };
        client.maintain_chunks(8);
        client.send_ping();
        Ok(client)
    }

    /// Server metadata captured during the handshake.
    pub fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    /// Endpoint exactly as supplied by the caller.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Local connection id.
    pub fn local_id(&self) -> u64 {
        self.local_id
    }

    /// Local registered name.
    pub fn local_name(&self) -> &str {
        &self.player_name
    }

    /// Current lifecycle state.
    pub fn state(&self) -> &NetState {
        &self.state
    }

    /// Whether the connection is still usable.
    pub fn is_connected(&self) -> bool {
        self.state.is_connected()
    }

    /// Whether the spawn area has finished loading.
    pub fn is_ready(&self) -> bool {
        self.tracker.is_ready()
    }

    /// `(loaded, target)` chunk counts for the loading overlay.
    pub fn loading_progress(&self) -> (usize, usize) {
        self.tracker.progress()
    }

    /// Number of replicated chunk columns.
    pub fn loaded_chunks(&self) -> usize {
        self.world.chunk_count()
    }

    /// Current chunk radius.
    pub fn render_radius(&self) -> i32 {
        self.tracker.radius()
    }

    /// Changes the requested chunk radius.
    pub fn set_render_radius(&mut self, radius: i32) {
        self.tracker.set_radius(radius);
    }

    /// The replicated world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Mutable replicated world access for the mesh scheduler and prediction.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// The local player.
    pub fn player(&self) -> &Player {
        &self.player
    }

    /// Mutable local player access for mouse look.
    pub fn player_mut(&mut self) -> &mut Player {
        &mut self.player
    }

    /// Estimated server tick.
    ///
    /// The interim protocol only sends the server tick once, in `Hello`, so the
    /// client advances it from wall-clock time since joining. This is used for
    /// day/night presentation only; it is not authoritative and does not stop
    /// while menus are open.
    pub fn server_tick(&self) -> u64 {
        self.info
            .tick
            .saturating_add((self.joined_at.elapsed().as_secs_f64() * 20.0) as u64)
    }

    /// Local simulation ticks since connecting.
    pub fn session_ticks(&self) -> u64 {
        self.session_ticks
    }

    /// Last measured round-trip time, if a pong has arrived.
    pub fn ping_ms(&self) -> Option<u32> {
        self.ping_ms
    }

    /// Most recent `PlayerState` for the local client (ignored for rendering).
    pub fn last_server_state(&self) -> Option<&PlayerSnapshot> {
        self.last_server_state.as_ref()
    }

    /// Authoritative survival state (vitals, inventory, equipment, selection).
    pub fn survival(&self) -> &SurvivalState {
        &self.survival
    }

    /// The authoritative 36-slot player inventory (hotbar `0..9`, main
    /// `9..36`).
    pub fn inventory(&self) -> &[Option<ItemStack>; ob_protocol::INVENTORY_SLOTS] {
        &self.survival.inventory
    }

    /// The authoritative equipment slots.
    pub fn equipment(&self) -> &[Option<ItemStack>; ob_protocol::EQUIPMENT_SLOTS] {
        &self.survival.equipment
    }

    /// Selected hotbar slot.
    pub fn selected_slot(&self) -> usize {
        self.survival.selected
    }

    /// Selected hotbar stack.
    pub fn selected_stack(&self) -> Option<ItemStack> {
        self.survival.selected_stack()
    }

    /// Current health.
    pub fn health(&self) -> f32 {
        self.survival.health
    }

    /// Current hunger.
    pub fn hunger(&self) -> f32 {
        self.survival.hunger
    }

    /// Effective armor defense points.
    pub fn armor_defense(&self) -> u8 {
        self.survival.armor_defense
    }

    /// Whether the server has declared the local player dead.
    pub fn is_dead(&self) -> bool {
        self.survival.dead
    }

    /// Death cause, if dead.
    pub fn death_cause(&self) -> Option<&str> {
        self.survival.death_cause.as_deref()
    }

    /// Replicated mob and item entities.
    pub fn entities(&self) -> &ReplicatedEntities {
        &self.entities
    }

    /// The open server container window and cursor preview.
    pub fn containers(&self) -> &RemoteContainers {
        &self.containers
    }

    /// The drawn container snapshot with the cursor preview subtracted.
    pub fn container_snapshot(&self) -> Option<ContainerSnapshot> {
        self.containers.snapshot(&self.survival.inventory)
    }

    /// Seconds since the connection was installed (deterministic item spin).
    pub fn session_seconds(&self) -> f32 {
        self.joined_at.elapsed().as_secs_f32()
    }

    /// `(mobs, items)` currently replicated.
    pub fn entity_counts(&self) -> (usize, usize) {
        (self.entities.mob_count(), self.entities.item_count())
    }

    /// Interpolated mob and item cubes for the renderer, in sorted-id order.
    pub fn entity_instances(&self, now: Instant) -> Vec<EntityInstance> {
        let elapsed = self.session_seconds();
        let mut instances = self.entities.mob_instances(&self.registry, now, elapsed);
        instances.extend(self.entities.item_instances(&self.registry, now, elapsed));
        instances
    }

    /// Mob under the crosshair, using replicated interpolated positions.
    pub fn mob_target(&self) -> Option<u64> {
        let now = Instant::now();
        self.entities.raycast_mob(
            &self.registry,
            self.player.eye(),
            self.player.forward(),
            MOB_ATTACK_REACH,
            now,
        )
    }

    /// Effective seconds to break `pos` with the authoritative selected item.
    ///
    /// A no-op block or unloaded column returns `0.0`, so the caller can skip
    /// the mining timer without inventing a Host.
    pub fn mining_seconds(&self, pos: BlockPos) -> f32 {
        if self.world.chunk(pos.chunk()).is_none() {
            return 0.0;
        }
        let block = self.world.get_block(pos);
        let held = self.survival.selected_stack().map(|stack| stack.item);
        self.registry.effective_mining_seconds(block, held)
    }

    /// Number of unconfirmed block predictions.
    pub fn pending_predictions(&self) -> usize {
        self.predictions.len()
    }

    /// Whether a container window is open.
    pub fn container_open(&self) -> bool {
        self.containers.window().is_some()
    }

    /// Sends `select_slot`; the HUD highlight follows the server's echo.
    pub fn select_slot(&mut self, slot: usize) -> bool {
        if !self.state.is_connected()
            || self.survival.dead
            || slot >= ob_protocol::HOTBAR_SLOTS
            || !self.select_limit.allow_now()
        {
            return false;
        }
        self.queue(ClientMessage::SelectSlot { slot: slot as u8 })
    }

    /// Sends `use_block` for a furnace/chest under the crosshair.
    pub fn use_block(&mut self, pos: BlockPos) -> bool {
        if !self.state.is_connected() || self.survival.dead || !pos.in_world() || !self.use_limit.allow_now() {
            return false;
        }
        self.queue(ClientMessage::UseBlock {
            pos: [pos.x, pos.y, pos.z],
        })
    }

    /// Sends `attack` for an explicit replicated mob id.
    pub fn send_attack(&mut self, target: Option<u64>) -> bool {
        if !self.state.is_connected() || self.survival.dead || !self.attack_limit.allow_now() {
            return false;
        }
        self.queue(ClientMessage::Attack { target })
    }

    /// Sends `inventory_move` with an optional window revision.
    pub fn send_inventory_move(&mut self, from: u16, to: u16, count: u8, revision: Option<u64>) -> bool {
        if !self.state.is_connected()
            || self.survival.dead
            || count == 0
            || from == to
            || !self.inventory_limit.allow_now()
        {
            return false;
        }
        self.queue(ClientMessage::InventoryMove {
            from,
            to,
            count,
            revision,
        })
    }

    /// Sends `eat` for the selected edible stack.
    pub fn send_eat(&mut self) -> bool {
        if !self.state.is_connected() || self.survival.dead || !self.eat_limit.allow_now() {
            return false;
        }
        self.queue(ClientMessage::Eat)
    }

    /// Sends `equip` for one hotbar armor stack.
    pub fn send_equip(&mut self, hotbar_slot: usize) -> bool {
        if !self.state.is_connected()
            || self.survival.dead
            || hotbar_slot >= ob_protocol::HOTBAR_SLOTS
            || !self.equip_limit.allow_now()
        {
            return false;
        }
        self.queue(ClientMessage::Equip {
            hotbar_slot: hotbar_slot as u8,
        })
    }

    /// Sends `respawn` once while dead.
    pub fn send_respawn(&mut self) -> bool {
        if !self.state.is_connected()
            || !self.survival.dead
            || self.survival.respawn_requested
            || !self.respawn_limit.allow_now()
        {
            return false;
        }
        if !self.queue(ClientMessage::Respawn) {
            return false;
        }
        self.survival.respawn_requested = true;
        true
    }

    /// Sends `container_close` and clears the local window/cursor.
    pub fn send_container_close(&mut self) -> bool {
        if !self.state.is_connected() || !self.close_limit.allow_now() {
            return false;
        }
        self.containers.close_current();
        self.container_awaiting_sync = false;
        self.container_cursor_rollback = None;
        self.queue(ClientMessage::ContainerClose)
    }

    /// Plans and sends one container click.
    ///
    /// The protocol has no server-side cursor slot, so the cursor is a local
    /// preview and placing it emits one `inventory_move`. A failed queue
    /// restores the previous preview.
    pub fn container_click(&mut self, slot: SlotId, button: ClickButton) -> RemoteClickOutcome {
        if !self.state.is_connected() || self.survival.dead {
            return RemoteClickOutcome {
                transfer: None,
                message: Some("Not connected".to_string()),
            };
        }
        if self.containers.window().is_none() {
            return RemoteClickOutcome {
                transfer: None,
                message: Some("No container is open".to_string()),
            };
        }
        if self.container_awaiting_sync {
            return RemoteClickOutcome {
                transfer: None,
                message: Some("Waiting for the server to confirm the last item move".to_string()),
            };
        }
        let inventory = self.survival.inventory;
        let previous_cursor = self.containers.cursor();
        let mut outcome = self.containers.click(&self.registry, &inventory, slot, button);
        if let Some(transfer) = outcome.transfer {
            if self.send_inventory_move(transfer.from, transfer.to, transfer.count, transfer.revision) {
                self.container_awaiting_sync = true;
                self.container_cursor_rollback = Some(previous_cursor);
            } else {
                self.containers.set_cursor(previous_cursor);
                outcome.transfer = None;
                outcome.message = Some("Could not send the item move".to_string());
            }
        }
        outcome
    }

    /// Equips the armor in any player inventory slot (`0..36`).
    ///
    /// Hotbar pieces use the compact `Equip` intent; a piece in the main
    /// inventory is moved to its equipment address with `InventoryMove`.
    pub fn equip_inventory_slot(&mut self, slot: usize) -> bool {
        if !self.state.is_connected() || self.survival.dead || slot >= ob_protocol::INVENTORY_SLOTS {
            return false;
        }
        let Some(stack) = self.survival.inventory[slot].filter(|stack| stack.count > 0) else {
            return false;
        };
        let Some(armor) = self.registry.item(stack.item).armor else {
            return false;
        };
        let Some(address) = crate::net_state::equipment_address(crate::inventory::armor_index(armor.slot)) else {
            return false;
        };
        if slot < ob_protocol::HOTBAR_SLOTS {
            self.send_equip(slot)
        } else {
            self.send_inventory_move(slot as u16, address, 1, None)
        }
    }

    /// Shift-click quick transfer between the hotbar and main inventory.
    ///
    /// Computes the first compatible/empty destination with the same helper the
    /// single-player path uses, then sends exactly one `InventoryMove`.
    pub fn quick_transfer_inventory(&mut self, slot: usize) -> bool {
        if !self.state.is_connected() || self.survival.dead {
            return false;
        }
        let Some((target, count)) =
            crate::inventory::quick_transfer_target(&self.registry, &self.survival.inventory, slot)
        else {
            return false;
        };
        self.send_inventory_move(slot as u16, target as u16, count, None)
    }

    /// Equips the armor in a player inventory slot or unequips a matching armor
    /// slot.
    ///
    /// Unequipping uses an `inventory_move` from the equipment address into the
    /// first empty player inventory slot.
    pub fn toggle_equipment(&mut self, armor_index: usize) -> bool {
        if !self.state.is_connected() || self.survival.dead || armor_index >= ob_protocol::EQUIPMENT_SLOTS {
            return false;
        }
        if self.survival.equipment[armor_index].is_none() {
            return false;
        }
        let Some(target) = self
            .survival
            .inventory
            .iter()
            .position(|slot| slot.is_none())
            .or_else(|| (self.survival.inventory[self.survival.selected].is_none()).then_some(self.survival.selected))
        else {
            return false;
        };
        let Some(from) = crate::net_state::equipment_address(armor_index) else {
            return false;
        };
        self.send_inventory_move(from, target as u16, 1, None)
    }

    /// Chat history including server-relayed lines.
    pub fn chat_log(&self) -> &ChatLog {
        &self.chat_log
    }

    /// Chat text input.
    pub fn chat_input(&self) -> &ChatInput {
        &self.chat_input
    }

    /// Mutable chat text input for the UI key handler.
    pub fn chat_input_mut(&mut self) -> &mut ChatInput {
        &mut self.chat_input
    }

    /// Advances chat fade timers.
    pub fn tick(&mut self, dt_seconds: f32) {
        self.chat_log.tick(dt_seconds);
    }

    /// Remote players sorted by connection id (for the Tab overlay and avatars).
    pub fn player_entries(&self) -> Vec<PlayerEntry> {
        let mut entries: Vec<PlayerEntry> = self
            .remote
            .values()
            .map(|player| PlayerEntry {
                id: player.id,
                name: player.name.clone(),
                is_self: false,
            })
            .collect();
        entries.push(PlayerEntry {
            id: self.local_id,
            name: self.player_name.clone(),
            is_self: true,
        });
        entries.sort_by_key(|entry| entry.id);
        entries
    }

    /// Number of players including the local client.
    pub fn player_count(&self) -> usize {
        self.remote.len() + 1
    }

    /// Interpolated remote players ready for [`avatar_instances`].
    pub fn avatar_states(&self, now: Instant) -> Vec<AvatarState> {
        let mut players: Vec<&RemotePlayer> = self.remote.values().collect();
        players.sort_by_key(|player| player.id);
        players
            .into_iter()
            .map(|player| {
                let (pos, yaw, _pitch) = player.interpolated(now);
                AvatarState {
                    id: player.id,
                    name: player.name.clone(),
                    pos,
                    yaw,
                }
            })
            .collect()
    }

    /// Drains inbound network events without blocking.
    ///
    /// Applies player-state snapshots, joins/leaves, block edits, chat and
    /// chunk columns, advances the loading state, expires unconfirmed
    /// predictions and schedules pings. Returns UI events for this frame.
    pub fn poll(&mut self) -> Vec<NetEvent> {
        let mut events = Vec::new();
        loop {
            match self.inbound.try_recv() {
                Ok(Inbound::Message(message)) => {
                    self.last_inbound = Instant::now();
                    if self.state.is_connected() {
                        self.handle_message(*message, &mut events);
                    }
                    for damage in self.survival.drain_damage_events() {
                        events.push(NetEvent::Damage {
                            amount: damage.amount,
                            cause: damage.cause,
                        });
                    }
                }
                Ok(Inbound::Chunk(column)) => {
                    self.last_inbound = Instant::now();
                    self.handle_chunk(*column);
                }
                Ok(Inbound::Closed(reason)) => self.handle_disconnect(reason, &mut events),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.state.is_connected() {
                        self.handle_disconnect("connection closed".to_string(), &mut events);
                    }
                    break;
                }
            }
        }
        let now = Instant::now();
        self.expire_predictions(now);
        if self.state.is_connected() && now.saturating_duration_since(self.last_inbound) >= IDLE_TIMEOUT {
            self.handle_disconnect(
                format!("no server frames for {} s", IDLE_TIMEOUT.as_secs()),
                &mut events,
            );
        }
        if self.state.is_connected() {
            if self
                .pending_ping
                .is_some_and(|(_, sent)| now.saturating_duration_since(sent) >= PING_TIMEOUT)
            {
                self.pending_ping = None;
                self.ping_ms = None;
            }
            if self.pending_ping.is_none()
                && now.saturating_duration_since(self.last_ping) >= PING_INTERVAL
                && self.ping_limit.allow(now)
            {
                self.send_ping_at(now);
            }
        }
        events
    }

    /// Requests missing chunks in the current interest ring.
    ///
    /// Up to `budget` requests per call are queued and deduplicated; callers
    /// normally pass 2–4 from the frame loop. Returns how many frames were
    /// queued.
    pub fn maintain_chunks(&mut self, budget: usize) -> usize {
        if !self.state.is_connected() {
            return 0;
        }
        let center = chunk_of(self.player.pos);
        self.tracker.set_center(center);
        let now = Instant::now();
        let mut sent = 0;
        for request in self.tracker.next_requests(budget, now) {
            if !self.request_limit.allow(now) {
                break;
            }
            if self.queue(ClientMessage::ChunkRequest {
                cx: request.x,
                cz: request.z,
            }) {
                self.tracker.mark_requested(request, now);
                sent += 1;
            }
        }
        sent
    }

    /// Evicts replicated columns farther than the current interest ring plus
    /// `margin` chunks, returning the removed positions.
    ///
    /// The network runtime calls this after a radius change or while moving so
    /// world memory, GPU section buffers and merged LOD batches stay bounded.
    /// The server's `sent_chunks` set is unchanged; a column that re-enters the
    /// ring is requested again and replaced normally.
    pub fn unload_far_chunks(&mut self, margin: i32) -> Vec<ChunkPos> {
        let center = self.tracker.center();
        let radius = self.tracker.radius() + margin.max(0);
        let stale: Vec<ChunkPos> = self
            .world
            .chunks()
            .map(|(pos, _)| *pos)
            .filter(|pos| (pos.x - center.x).abs() > radius || (pos.z - center.z).abs() > radius)
            .collect();
        for pos in &stale {
            self.world.remove_chunk(*pos);
            self.tracker.mark_unloaded(*pos);
        }
        stale
    }

    /// Steps the local player against the replicated world and sends `Move`
    /// when position or look angles changed.
    pub fn tick_player(&mut self, input: PlayerInput) -> StepEvents {
        if !self.state.is_connected() || self.survival.dead {
            return StepEvents::default();
        }
        if !terrain_ready(&self.world, self.player.pos) {
            // Freeze over columns that have not arrived yet instead of falling
            // through unloaded space at a streaming edge. The request ring is
            // centred here, so the 3x3 area arrives shortly.
            return StepEvents::default();
        }
        let events = ob_sim::step_player(&self.world, &mut self.player, &input, TICK_DT);
        self.session_ticks += 1;
        if self.tracker.is_ready() {
            self.maybe_send_move();
        }
        events
    }

    /// Sends one `Move` immediately, bypassing the change check and rate limit.
    ///
    /// Used by integration tests; the frame loop uses [`Self::tick_player`].
    pub fn send_move_now(&mut self, pos: [f64; 3], yaw: f32, pitch: f32) -> bool {
        if !self.state.is_connected()
            || !pos.iter().all(|value| value.is_finite())
            || !yaw.is_finite()
            || !pitch.is_finite()
        {
            return false;
        }
        let sent = self.queue(ClientMessage::Move { pos, yaw, pitch });
        if sent {
            self.last_sent_move = Some((pos, yaw, pitch));
        }
        sent
    }

    /// Predicts and sends one block edit.
    ///
    /// Returns `false` when the connection is closed, the chunk is not loaded,
    /// the edit rate limit is hit or the outbound queue is full. Local
    /// prediction is reverted if the intent cannot be queued.
    pub fn edit_block(&mut self, pos: BlockPos, block: BlockId) -> bool {
        if !self.state.is_connected()
            || self.survival.dead
            || !pos.in_world()
            || block.0 as usize >= self.registry.len()
        {
            return false;
        }
        let current = self.world.get_block(pos);
        if current == block {
            return false;
        }
        if self.world.chunk(pos.chunk()).is_none() {
            return false;
        }
        if block == BlockId::AIR {
            if current == BlockId::AIR || !self.registry.block(current).breakable() {
                return false;
            }
        } else if current != BlockId::AIR || self.registry.block(block).fluid {
            return false;
        }
        if !self.edit_limit.allow_now() {
            return false;
        }
        // Keep the original value when a block is edited repeatedly before the
        // server confirms either intent, so a rejection can still restore it.
        let previous = self
            .predictions
            .entries
            .get(&pos)
            .map_or(current, |prediction| prediction.previous);
        self.world.set_block(pos, block);
        self.predictions.remember(pos, previous, block, Instant::now());
        if !self.queue(ClientMessage::BlockEdit {
            pos: [pos.x, pos.y, pos.z],
            block: block.0,
        }) {
            self.world.set_block(pos, current);
            self.predictions.confirm(&pos);
            return false;
        }
        true
    }

    /// Breaks the block under the crosshair, with local prediction.
    pub fn break_block(&mut self) -> Option<(BlockPos, BlockId)> {
        if !self.state.is_connected() || self.survival.dead {
            return None;
        }
        let hit = self.world.raycast(self.player.eye(), self.player.forward(), REACH)?;
        let block = self.world.get_block(hit.block);
        if block == BlockId::AIR || !self.registry.block(block).breakable() {
            return None;
        }
        self.edit_block(hit.block, BlockId::AIR).then_some((hit.block, block))
    }

    /// Places `block` on the face under the crosshair, with local prediction.
    pub fn place_block(&mut self, block: BlockId) -> Option<BlockPos> {
        if !self.state.is_connected()
            || self.survival.dead
            || block == BlockId::AIR
            || block.0 as usize >= self.registry.len()
        {
            return None;
        }
        let hit = self.world.raycast(self.player.eye(), self.player.forward(), REACH)?;
        if !self.world.is_solid(hit.block) {
            return None;
        }
        let target = hit.block.offset(hit.normal.x, hit.normal.y, hit.normal.z);
        if !target.in_world() || self.world.get_block(target) != BlockId::AIR {
            return None;
        }
        if Aabb::block(target).intersects(&self.player.aabb()) {
            return None;
        }
        self.edit_block(target, block).then_some(target)
    }

    /// Sends a chat line after local validation and the 5-per-10-seconds limit.
    ///
    /// The server relays the line back, so this does not append locally.
    pub fn send_chat(&mut self, text: &str) -> Result<(), String> {
        if !self.state.is_connected() {
            return Err("not connected".to_string());
        }
        let text = validate_chat_text(text).map_err(|error| error.to_string())?;
        if !self.chat_limit.allow_now() {
            return Err("chat rate-limited (5 lines per 10 seconds)".to_string());
        }
        if !self.queue(ClientMessage::Chat { text }) {
            return Err("outbound queue is full".to_string());
        }
        Ok(())
    }

    /// Sends one ping immediately.
    pub fn send_ping(&mut self) -> bool {
        let now = Instant::now();
        if !self.state.is_connected() || !self.ping_limit.allow(now) {
            return false;
        }
        self.send_ping_at(now)
    }

    /// Queues the final `Disconnect`, flushes it, then joins both threads.
    ///
    /// Idempotent; also called from `Drop`. Never panics when the peer has
    /// already gone away.
    pub fn shutdown(&mut self) {
        if self.reader.is_none() && self.writer.is_none() {
            return;
        }
        self.shutdown.store(true, Ordering::SeqCst);
        if self.state.is_connected()
            && let Ok(frame) = encode_client_frame(&ClientMessage::Disconnect)
            && let Some(sender) = &self.outbound
        {
            let _ = sender.try_send(frame);
        }
        self.outbound.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.state.disconnect("client closed the connection");
    }

    /// Sends one `Move` if the player moved or turned since the last update.
    fn maybe_send_move(&mut self) {
        let pos = [self.player.pos.x, self.player.pos.y, self.player.pos.z];
        let yaw = self.player.yaw;
        let pitch = self.player.pitch;
        if let Some((last_pos, last_yaw, last_pitch)) = self.last_sent_move {
            let moved = (pos[0] - last_pos[0]).abs() > 1e-8
                || (pos[1] - last_pos[1]).abs() > 1e-8
                || (pos[2] - last_pos[2]).abs() > 1e-8;
            let turned = (yaw - last_yaw).abs() > 1e-5 || (pitch - last_pitch).abs() > 1e-5;
            if !moved && !turned {
                return;
            }
        }
        if !self.move_limit.allow_now() {
            return;
        }
        if self.queue(ClientMessage::Move { pos, yaw, pitch }) {
            self.last_sent_move = Some((pos, yaw, pitch));
        }
    }

    /// Queues one message on the bounded writer channel.
    fn queue(&mut self, message: ClientMessage) -> bool {
        if !self.state.is_connected() {
            return false;
        }
        let frame = match encode_client_frame(&message) {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!("could not encode client message: {error}");
                return false;
            }
        };
        let Some(sender) = self.outbound.as_ref() else {
            return false;
        };
        match sender.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                log::warn!("client outbound queue is full; dropping one message");
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Schedules a ping nonce and remembers when it was sent.
    fn send_ping_at(&mut self, now: Instant) -> bool {
        let id = self.next_ping_id;
        self.next_ping_id = self.next_ping_id.wrapping_add(1);
        if !self.queue(ClientMessage::Ping { id }) {
            return false;
        }
        self.last_ping = now;
        self.pending_ping = Some((id, now));
        true
    }

    /// Applies one server message.
    fn handle_message(&mut self, message: ServerMessage, events: &mut Vec<NetEvent>) {
        match message {
            ServerMessage::PlayerState {
                id,
                name,
                pos,
                yaw,
                pitch,
            } => {
                let Some(position) = sanitise_player_pos(pos) else {
                    return;
                };
                let pos = [position.x, position.y, position.z];
                if id == self.local_id {
                    self.last_server_state = Some(PlayerSnapshot {
                        id,
                        name,
                        pos,
                        yaw,
                        pitch,
                    });
                    return;
                }
                let now = Instant::now();
                match self.remote.get_mut(&id) {
                    Some(player) => player.update(pos, yaw, pitch, now),
                    None => {
                        if self.remote.len() >= MAX_PLAYER_LIST {
                            return;
                        }
                        let snapshot = PlayerSnapshot {
                            id,
                            name: name.clone(),
                            pos,
                            yaw,
                            pitch,
                        };
                        self.remote.insert(id, RemotePlayer::new(&snapshot, now));
                        events.push(NetEvent::Joined { id, name });
                    }
                }
            }
            ServerMessage::Join { id, name } => {
                if id == self.local_id {
                    return;
                }
                if !self.remote.contains_key(&id) {
                    if self.remote.len() >= MAX_PLAYER_LIST {
                        return;
                    }
                    let spawn = [self.info.spawn.x, self.info.spawn.y, self.info.spawn.z];
                    let snapshot = PlayerSnapshot {
                        id,
                        name: name.clone(),
                        pos: spawn,
                        yaw: 0.0,
                        pitch: 0.0,
                    };
                    self.remote.insert(id, RemotePlayer::new(&snapshot, Instant::now()));
                    events.push(NetEvent::Joined { id, name });
                }
            }
            ServerMessage::Leave { id } => {
                if let Some(player) = self.remote.remove(&id) {
                    events.push(NetEvent::Left { id, name: player.name });
                }
            }
            ServerMessage::PlayerList { players } => {
                let now = Instant::now();
                let mut next = HashMap::with_capacity(players.len().min(MAX_PLAYER_LIST));
                // Drop extras instead of building an unbounded roster or doing
                // unbounded interpolation/UI work for a hostile broadcast.
                for snapshot in players.into_iter().take(MAX_PLAYER_LIST) {
                    if snapshot.id == self.local_id || !finite_snapshot(snapshot.pos, snapshot.yaw, snapshot.pitch) {
                        continue;
                    }
                    let Some(position) = sanitise_player_pos(snapshot.pos) else {
                        continue;
                    };
                    let snapshot = PlayerSnapshot {
                        pos: [position.x, position.y, position.z],
                        ..snapshot
                    };
                    next.insert(snapshot.id, RemotePlayer::new(&snapshot, now));
                }
                self.remote = next;
            }
            ServerMessage::BlockEdit { pos, block, player } => {
                let _ = player;
                if block as usize >= self.registry.len() {
                    return;
                }
                let block_pos = BlockPos::new(pos[0], pos[1], pos[2]);
                if !block_pos.in_world() {
                    return;
                }
                self.predictions.confirm(&block_pos);
                self.apply_authoritative_edit(block_pos, BlockId(block));
            }
            ServerMessage::Chat { from, text } => {
                self.chat_log.push(from, text);
                // Mirror only the bounded/sanitised line into the event so the
                // UI never handles the raw wire strings.
                if let Some(line) = self.chat_log.lines().last() {
                    events.push(NetEvent::Chat {
                        from: line.from.clone(),
                        text: line.text.clone(),
                    });
                }
            }
            ServerMessage::Pong { id } => {
                if let Some((pending, sent)) = self.pending_ping
                    && pending == id
                {
                    let millis = sent.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                    self.ping_ms = Some(millis);
                    self.pending_ping = None;
                }
            }
            ServerMessage::Disconnect { reason } => {
                self.handle_disconnect(reason, events);
            }
            ServerMessage::Hello { .. } => {
                self.handle_disconnect("protocol_error: duplicate hello".to_string(), events);
            }
            ServerMessage::ChunkData { .. } => {
                // Chunk bytes are decoded on the reader thread and never reach
                // this path; ignore a stray frame rather than guessing.
            }
            ServerMessage::PlayerVitals {
                health,
                hunger,
                armor_defense,
            } => {
                self.survival.apply_vitals(health, hunger, armor_defense);
                self.player.health = self.survival.health;
                self.player.hunger = self.survival.hunger;
            }
            ServerMessage::InventorySync {
                hotbar,
                main,
                selected,
                equipment,
            } => {
                self.container_awaiting_sync = false;
                self.container_cursor_rollback = None;
                self.survival
                    .apply_inventory(&self.registry, &hotbar, &main, selected, &equipment);
                self.player.health = self.survival.health;
                self.player.hunger = self.survival.hunger;
                if self.containers.apply_hotbar(&self.survival.inventory) {
                    events.push(NetEvent::ContainerCursorDropped);
                }
            }
            ServerMessage::SelectedSlot { slot } => self.survival.apply_selected(slot),
            ServerMessage::ContainerOpen {
                window,
                kind,
                pos,
                revision,
                slots,
                furnace,
            } => {
                self.container_awaiting_sync = false;
                self.container_cursor_rollback = None;
                self.containers.open(
                    window,
                    kind,
                    BlockPos::new(pos[0], pos[1], pos[2]),
                    revision,
                    slots,
                    furnace,
                    &self.registry,
                );
                events.push(NetEvent::ContainerOpened { window });
            }
            ServerMessage::ContainerUpdate {
                window,
                revision,
                slots,
                furnace,
            } => {
                let update = self.containers.update(&self.registry, window, revision, slots, furnace);
                if update.cursor_dropped {
                    events.push(NetEvent::ContainerCursorDropped);
                }
            }
            ServerMessage::ContainerClose { window, reason } => {
                self.container_awaiting_sync = false;
                self.container_cursor_rollback = None;
                if self.containers.close(window) {
                    events.push(NetEvent::ContainerClosed { window, reason });
                }
            }
            ServerMessage::EntitySpawn {
                id,
                kind,
                pos,
                yaw,
                health,
                hurt,
            } => {
                self.entities.apply_spawn(
                    &self.registry,
                    ob_protocol::EntitySnapshot {
                        id,
                        kind,
                        pos,
                        yaw,
                        health,
                        hurt,
                    },
                    Instant::now(),
                );
            }
            ServerMessage::EntitySnapshot { entities } => {
                self.entities.apply_snapshot(&self.registry, entities, Instant::now());
            }
            ServerMessage::EntityState { entities } => {
                self.entities.apply_state(&self.registry, &entities, Instant::now());
            }
            ServerMessage::EntityDespawn { ids, reason } => {
                self.entities.apply_despawn(&ids, reason);
            }
            ServerMessage::DamageEvent { amount, cause } => {
                self.survival.apply_damage(amount, &cause);
            }
            ServerMessage::Death { cause } => {
                let window = self.containers.window_id();
                self.containers.close_current();
                self.container_awaiting_sync = false;
                self.container_cursor_rollback = None;
                self.survival.apply_death(&cause);
                self.player.dead = true;
                self.player.health = 0.0;
                if let Some(window) = window {
                    events.push(NetEvent::ContainerClosed {
                        window,
                        reason: "death".to_string(),
                    });
                }
                events.push(NetEvent::Death { cause });
            }
            ServerMessage::Respawn { pos, health, hunger } => {
                let fallback = self
                    .last_server_state
                    .as_ref()
                    .map(|snapshot| WorldVec::new(snapshot.pos[0], snapshot.pos[1], snapshot.pos[2]))
                    .unwrap_or(self.player.pos);
                let position = resolve_respawn_pos(pos, fallback);
                self.survival.apply_respawn(health, hunger);
                self.player.pos = position;
                self.player.velocity = WorldVec::ZERO;
                self.player.on_ground = false;
                self.player.dead = false;
                self.player.health = self.survival.health;
                self.player.hunger = self.survival.hunger;
                self.last_sent_move = None;
                events.push(NetEvent::Respawned {
                    pos: [position.x, position.y, position.z],
                    health,
                    hunger,
                });
            }
            ServerMessage::AttackResult {
                target,
                hit,
                damage,
                killed,
            } => events.push(NetEvent::AttackResult {
                target,
                hit,
                damage,
                killed,
            }),
            ServerMessage::ActionRejected { action, reason } => {
                if action == "block_edit" {
                    self.revert_latest_prediction();
                }
                if action == "inventory_move" {
                    self.container_awaiting_sync = false;
                    if let Some(cursor) = self.container_cursor_rollback.take() {
                        self.containers.set_cursor(cursor);
                    }
                }
                if action == "respawn" {
                    // Let the death screen retry instead of staying stuck on a
                    // rejected request.
                    self.survival.respawn_requested = false;
                }
                events.push(NetEvent::ActionRejected {
                    hint: friendly_action_rejection(&action, &reason),
                    action,
                    reason,
                });
            }
            // Unknown future variants are ignored on purpose so a newer server
            // cannot break this client (see spec 18 §3.13.9).
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }

    /// Inserts a decoded column and replays edits that raced ahead of it.
    fn handle_chunk(&mut self, column: ChunkColumn) {
        if !self.state.is_connected() {
            return;
        }
        let pos = column.pos();
        if self.world.chunk(pos).is_none() {
            self.world.insert_chunk(column);
            mark_neighbour_chunks_dirty(&mut self.world, pos);
            if let Some(edits) = self.pending_edits.remove(&pos) {
                self.pending_edit_count = self.pending_edit_count.saturating_sub(edits.len());
                for (block_pos, block) in edits {
                    self.world.set_block(block_pos, block);
                }
            }
        }
        self.tracker.mark_loaded(pos);
        if self.state == NetState::Loading && self.tracker.is_ready() {
            self.state = NetState::Playing;
            log::info!(
                "spawn area ready: {}/{} chunks, {} total",
                self.tracker.loaded_count(),
                self.tracker.target_count(),
                self.world.chunk_count()
            );
        }
    }

    /// Applies a server-authoritative block edit, queueing it when unloaded.
    fn apply_authoritative_edit(&mut self, pos: BlockPos, block: BlockId) {
        if self.world.chunk(pos.chunk()).is_some() {
            self.world.set_block(pos, block);
            return;
        }
        if self.pending_edit_count >= MAX_PENDING_EDITS {
            log::warn!("dropping authoritative edit: too many edits buffered for unloaded chunks");
            return;
        }
        self.pending_edits.entry(pos.chunk()).or_default().push((pos, block));
        self.pending_edit_count += 1;
    }

    /// Reverts the newest unconfirmed prediction after an `action_rejected`.
    ///
    /// The wire rejection does not name the cell, so the client reverts the most
    /// recent prediction. A server broadcast confirms predictions before this
    /// path can run, so only still-pending edits are affected.
    fn revert_latest_prediction(&mut self) -> Option<(BlockPos, BlockId)> {
        let (pos, prediction) = self.predictions.revert_latest()?;
        if self.world.chunk(pos.chunk()).is_some() && self.world.get_block(pos) == prediction.placed {
            self.world.set_block(pos, prediction.previous);
        }
        Some((pos, prediction.previous))
    }

    /// Reverts predictions the server silently rejected.
    fn expire_predictions(&mut self, now: Instant) {
        for (pos, prediction) in self.predictions.expire(now) {
            if self.world.chunk(pos.chunk()).is_some() && self.world.get_block(pos) == prediction.placed {
                self.world.set_block(pos, prediction.previous);
            }
        }
    }

    /// Clears every piece of per-session replicated state.
    ///
    /// Called on disconnect so a stale HUD, inventory, entity or container can
    /// never be mistaken for live server state. A retry builds a new client,
    /// which starts from this same empty baseline.
    fn reset_session_state(&mut self) {
        self.survival.reset();
        self.entities.reset();
        self.containers.reset();
        self.container_awaiting_sync = false;
        self.container_cursor_rollback = None;
        self.predictions.clear();
        self.pending_edits.clear();
        self.pending_edit_count = 0;
        self.remote.clear();
        self.last_server_state = None;
        self.player.dead = false;
        self.player.health = 20.0;
        self.player.hunger = 20.0;
        self.last_sent_move = None;
    }

    /// Transitions to disconnected once and reports the event.
    fn handle_disconnect(&mut self, reason: String, events: &mut Vec<NetEvent>) {
        if self.state.disconnect(reason.clone()) {
            self.reset_session_state();
            log::info!("disconnected: {reason}");
            events.push(NetEvent::Disconnected { reason });
        }
    }
}

impl Drop for NetworkClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Validates a server `Hello` message against the local registry.
fn accept_hello(message: &ServerMessage, registry: &Registry) -> Result<ServerInfo, NetError> {
    let ServerMessage::Hello {
        protocol,
        world_name,
        seed,
        spawn,
        content_fingerprint: server_fingerprint,
        tick,
        day_ticks,
        max_players,
        motd,
        blocks,
    } = message
    else {
        return Err(NetError::Handshake("expected server hello".to_string()));
    };
    if *protocol != PROTOCOL_VERSION {
        return Err(NetError::ProtocolMismatch {
            server: *protocol,
            client: PROTOCOL_VERSION,
        });
    }
    let local = content_fingerprint(registry);
    let manifest_ok = manifest_matches(blocks, registry);
    if *server_fingerprint != local || !manifest_ok {
        return Err(NetError::ContentMismatch {
            server: *server_fingerprint,
            client: local,
            manifest: !manifest_ok,
        });
    }
    let Some(spawn) = sanitise_player_pos(*spawn) else {
        return Err(NetError::Handshake("server spawn is not finite".to_string()));
    };
    Ok(ServerInfo {
        world_name: world_name.clone(),
        seed: *seed,
        spawn,
        content_fingerprint: *server_fingerprint,
        tick: *tick,
        day_ticks: (*day_ticks).max(1),
        max_players: *max_players,
        motd: motd.clone(),
    })
}

/// Maps a handshake read failure, distinguishing a timed-out read.
fn map_handshake_error(error: ProtocolError) -> NetError {
    match &error {
        ProtocolError::Io(io_error)
            if matches!(io_error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
        {
            NetError::Timeout("handshake timed out".to_string())
        }
        _ => NetError::Protocol(error),
    }
}

/// Resolves a `HOST:PORT` endpoint into a small deduplicated address list.
fn resolve_endpoint(endpoint: &str) -> Result<Vec<SocketAddr>, NetError> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(NetError::InvalidEndpoint("endpoint is empty".to_string()));
    }
    let mut addresses = Vec::new();
    for address in endpoint.to_socket_addrs().map_err(|error| {
        NetError::InvalidEndpoint(format!("'{endpoint}' is not a valid HOST:PORT address ({error})"))
    })? {
        if !addresses.contains(&address) {
            addresses.push(address);
        }
        if addresses.len() >= 8 {
            break;
        }
    }
    if addresses.is_empty() {
        return Err(NetError::InvalidEndpoint(format!(
            "'{endpoint}' did not resolve to an address"
        )));
    }
    Ok(addresses)
}

/// Marks the four neighbouring loaded columns dirty after a new column arrives.
///
/// Mesh face culling reads neighbour blocks, so the shared borders must be
/// remeshed even though the new column's own sections were marked dirty by
/// [`World::insert_chunk`].
fn mark_neighbour_chunks_dirty(world: &mut World, pos: ChunkPos) {
    let min_section_y = WORLD_MIN_Y / ob_math::SECTION_SIZE as i32;
    for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let neighbour = ChunkPos::new(pos.x + dx, pos.z + dz);
        if world.chunk(neighbour).is_none() {
            continue;
        }
        for index in 0..N_SECTIONS {
            world.mark_section_dirty(SectionPos::new(neighbour.x, min_section_y + index as i32, neighbour.z));
        }
    }
}

/// Writer thread: drains encoded frames in order and closes the socket on error.
fn writer_loop(
    mut socket: TcpStream,
    receiver: Receiver<Vec<u8>>,
    inbound: SyncSender<Inbound>,
    shutdown: Arc<AtomicBool>,
) {
    while let Ok(frame) = receiver.recv() {
        if let Err(error) = socket.write_all(&frame) {
            log::debug!("client writer failed: {error}");
            let _ = push_inbound(
                &inbound,
                Inbound::Closed(format!("connection lost while sending: {error}")),
                &shutdown,
            );
            shutdown.store(true, Ordering::SeqCst);
            return;
        }
    }
    let _ = socket.flush();
}

/// Reader thread: decodes frames and chunk snapshots into the bounded queue.
fn reader_loop(
    mut reader: FrameReader,
    registry: Arc<Registry>,
    inbound: SyncSender<Inbound>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match reader.read_server_message(MAX_FRAME_LEN) {
            Ok(ServerMessage::ChunkData { cx, cz, bytes }) => match decode_chunk(&registry, cx, cz, &bytes) {
                Ok(column) => {
                    if !push_inbound(&inbound, Inbound::Chunk(Box::new(column)), &shutdown) {
                        return;
                    }
                }
                Err(error) => {
                    let _ = push_inbound(&inbound, Inbound::Closed(format!("chunk_decode: {error}")), &shutdown);
                    return;
                }
            },
            Ok(message) => {
                if !push_inbound(&inbound, Inbound::Message(Box::new(message)), &shutdown) {
                    return;
                }
            }
            Err(ProtocolError::Io(error))
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(error) => {
                if !shutdown.load(Ordering::SeqCst) {
                    let _ = push_inbound(
                        &inbound,
                        Inbound::Closed(format!("connection closed: {error}")),
                        &shutdown,
                    );
                }
                return;
            }
        }
    }
}

/// Pushes one inbound item, retrying while the bounded queue is full.
///
/// Returns `false` once the client is shutting down or the receiver is gone.
fn push_inbound(sender: &SyncSender<Inbound>, mut item: Inbound, shutdown: &AtomicBool) -> bool {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        match sender.try_send(item) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                item = returned;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Maps a raw `Disconnect` reason code to a user-facing sentence.
///
/// The machine-readable prefix stays visible when it is unknown, so a client
/// bug report can include it.
pub fn friendly_disconnect_reason(reason: &str) -> String {
    let (code, detail) = reason.split_once(':').unwrap_or((reason, ""));
    let detail = detail.trim();
    let base = match code.trim() {
        "server_full" => "The server is full.".to_string(),
        "name_taken" => "That player name is already in use.".to_string(),
        "not_whitelisted" => "This player name is not on the server whitelist.".to_string(),
        "server_shutdown" => "The server is shutting down.".to_string(),
        "protocol_mismatch" => "The server speaks a different protocol version.".to_string(),
        "invalid_name" => "The server rejected this player name.".to_string(),
        "rate_limited" => "The server rate-limited this connection.".to_string(),
        "protocol_error" => "The server rejected an invalid protocol message.".to_string(),
        _ => String::new(),
    };
    if base.is_empty() {
        reason.to_string()
    } else if detail.is_empty() {
        base
    } else {
        format!("{base} ({detail})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_pos(x: i32, z: i32) -> ChunkPos {
        ChunkPos::new(x, z)
    }

    #[test]
    fn chunk_tracker_deduplicates_requests_and_reports_progress() {
        let start = Instant::now();
        let mut tracker = ChunkTracker::new(1, chunk_pos(0, 0));
        assert_eq!(tracker.target_count(), 9);
        assert_eq!(tracker.progress(), (0, 9));
        assert!(!tracker.is_ready());

        let first = tracker.next_requests(4, start);
        assert_eq!(first.len(), 4);
        for pos in &first {
            tracker.mark_requested(*pos, start);
        }
        let second = tracker.next_requests(4, start);
        assert_eq!(second.len(), 4);
        assert!(
            second.iter().all(|pos| !first.contains(pos)),
            "requests are deduplicated"
        );
        for pos in &second {
            tracker.mark_requested(*pos, start);
        }
        let third = tracker.next_requests(4, start);
        assert_eq!(third.len(), 1, "the whole 3x3 ring is eventually requested");
        for pos in &third {
            tracker.mark_requested(*pos, start);
        }
        assert!(tracker.next_requests(4, start).is_empty());
        assert_eq!(tracker.pending_count(), 9);

        // A lost request is retried after the timeout.
        let retried = tracker.next_requests(4, start + CHUNK_REQUEST_TIMEOUT);
        assert_eq!(retried.len(), 4, "expired requests become candidates again");
        assert_eq!(tracker.pending_count(), 0, "expired requests are forgotten");

        // Spawn plus four neighbours makes the area playable before the whole
        // 3x3 ring has arrived.
        tracker.mark_loaded(chunk_pos(0, 0));
        for pos in [chunk_pos(1, 0), chunk_pos(-1, 0), chunk_pos(0, 1), chunk_pos(0, -1)] {
            tracker.mark_loaded(pos);
        }
        assert!(tracker.is_ready());
        assert_eq!(tracker.progress(), (5, 9));
        assert_eq!(tracker.pending_count(), 0, "loaded chunks are no longer pending");
    }

    #[test]
    fn chunk_tracker_recentres_without_forgetting_loaded_chunks() {
        let mut tracker = ChunkTracker::new(1, chunk_pos(0, 0));
        tracker.mark_loaded(chunk_pos(0, 0));
        tracker.mark_loaded(chunk_pos(1, 0));
        tracker.set_center(chunk_pos(1, 0));
        assert_eq!(tracker.target_count(), 9);
        assert_eq!(tracker.progress(), (2, 9));
        tracker.set_radius(2);
        assert_eq!(tracker.radius(), 2);
        assert_eq!(tracker.target_count(), 25);
        tracker.set_radius(99);
        assert_eq!(tracker.radius(), MAX_RENDER_RADIUS);
    }

    #[test]
    fn chat_log_caps_bounds_and_fades() {
        let mut log = ChatLog::default();
        for index in 0..(MAX_CHAT_LINES + 5) {
            log.push("alice", format!("line {index}"));
        }
        assert_eq!(log.len(), MAX_CHAT_LINES);
        assert_eq!(
            log.lines().next().map(|line| line.text.as_str()),
            Some("line 5"),
            "the oldest lines are dropped"
        );
        assert_eq!(log.visible(4).len(), 4);

        log.tick(CHAT_FADE_START - 0.1);
        assert_eq!(log.lines().next().map(ChatLine::alpha), Some(1.0));
        log.tick(1.0);
        let alpha = log.lines().next().map(ChatLine::alpha).unwrap_or(0.0);
        assert!(alpha > 0.0 && alpha < 1.0, "chat fades: {alpha}");
        log.tick(CHAT_FADE_SECONDS);
        assert!(log.visible(8).is_empty(), "fully faded lines are hidden");
        assert_eq!(ChatLine::new("a", "b").alpha(), 1.0);
    }

    #[test]
    fn chat_log_sanitises_truncates_and_bounds_memory() {
        let mut log = ChatLog::default();
        log.push("s".repeat(500), format!("a\nb\tc\rd\u{0}e{}", "x".repeat(10_000)));
        let line = log.lines().next().expect("retained line");
        assert_eq!(line.from.chars().count(), MAX_CHAT_SENDER_CHARS);
        assert!(line.from.ends_with("..."), "sender truncation is marked");
        assert_eq!(line.text.chars().count(), MAX_CHAT_TEXT_CHARS);
        assert!(line.text.ends_with("..."), "text truncation is marked");
        assert!(!line.from.chars().any(char::is_control));
        assert!(!line.text.chars().any(char::is_control));
        assert_eq!(
            line.text.chars().take(9).collect::<String>(),
            "a b c d e",
            "newlines/tabs/NUL become spaces"
        );
        assert_eq!(log.retained_bytes(), line.from.len() + line.text.len());
        assert!(log.retained_bytes() <= MAX_CHAT_BYTES);

        // Multi-byte scalars are truncated on a character boundary.
        log.clear();
        assert_eq!(log.retained_bytes(), 0);
        log.push(
            "名".repeat(MAX_CHAT_SENDER_CHARS + 20),
            "é".repeat(MAX_CHAT_TEXT_CHARS + 20),
        );
        let line = log.lines().next().expect("retained unicode line");
        assert_eq!(line.from.chars().count(), MAX_CHAT_SENDER_CHARS);
        assert_eq!(line.text.chars().count(), MAX_CHAT_TEXT_CHARS);
        assert!(line.text.ends_with("..."));
        assert_eq!(
            line.text.chars().filter(|character| *character == 'é').count(),
            MAX_CHAT_TEXT_CHARS - 3
        );
        assert!(line.from.chars().all(|character| character == '名' || character == '.'));

        // The whole log is bounded by bytes, not only by its line count.
        log.clear();
        let sender = "𝄞".repeat(MAX_CHAT_SENDER_CHARS);
        let text = "𝄞".repeat(MAX_CHAT_TEXT_CHARS);
        for _ in 0..(MAX_CHAT_LINES + 20) {
            log.push(sender.clone(), text.clone());
            assert!(log.len() <= MAX_CHAT_LINES);
            assert!(log.retained_bytes() <= MAX_CHAT_BYTES);
        }
        assert!(log.len() < MAX_CHAT_LINES, "the byte cap evicts before the line cap");
        assert!(
            log.lines()
                .all(|line| line.from.chars().count() == MAX_CHAT_SENDER_CHARS
                    && line.text.chars().count() == MAX_CHAT_TEXT_CHARS)
        );
    }

    #[test]
    fn respawn_positions_are_finite_and_bounded() {
        let fallback = WorldVec::new(12.5, 70.0, -4.5);
        let mut player = Player::new(fallback);
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let position = resolve_respawn_pos([invalid, invalid, invalid], player.pos);
            assert_eq!(position, fallback, "non-finite positions use the fallback");
            player.pos = position;
            assert!(player.pos.is_finite());
        }
        let huge = resolve_respawn_pos([f64::MAX, -f64::MAX, f64::MAX], player.pos);
        assert_eq!(huge.x, WORLD_BORDER_XZ);
        assert_eq!(huge.z, WORLD_BORDER_XZ);
        assert_eq!(huge.y, PLAYER_MIN_FEET_Y);
        assert!(huge.is_finite());
        player.pos = huge;
        assert!(player.pos.is_finite());
        assert!(player.pos.x.abs() <= WORLD_BORDER_XZ);
        assert!((PLAYER_MIN_FEET_Y..=PLAYER_MAX_FEET_Y).contains(&player.pos.y));
        assert!(player.pos.z.abs() <= WORLD_BORDER_XZ);

        let normal = resolve_respawn_pos([1.0, 64.0, 2.0], player.pos);
        assert_eq!(normal, WorldVec::new(1.0, 64.0, 2.0));
    }

    #[test]
    fn handshake_roster_is_capped_but_keeps_the_local_player() {
        let player = |id: u64| PlayerSnapshot {
            id,
            name: format!("player{id}"),
            pos: [0.5, 70.0, 0.5],
            yaw: 0.0,
            pitch: 0.0,
        };
        let roster: Vec<PlayerSnapshot> = (0..(MAX_PLAYER_LIST as u64 + 20)).map(player).collect();
        let local = MAX_PLAYER_LIST as u64 + 5;
        let capped = cap_handshake_roster(roster, Some(local), "player0");
        assert_eq!(capped.len(), MAX_PLAYER_LIST);
        assert!(capped.iter().any(|snapshot| snapshot.id == local), "self is preserved");

        let small = vec![player(1), player(2)];
        assert_eq!(cap_handshake_roster(small, Some(2), "player2").len(), 2);
    }

    #[test]
    fn chat_input_accepts_ascii_and_enforces_limits() {
        let mut input = ChatInput::default();
        assert!(!input.is_active());
        assert!(!input.push_char('x'));
        input.open();
        assert!(input.is_active());
        for character in "hello".chars() {
            assert!(input.push_char(character));
        }
        assert!(input.push_char(' '));
        assert!(!input.push_char('æ'), "non-ASCII is rejected");
        assert!(!input.push_char('\n'), "control characters are rejected");
        assert_eq!(input.text(), Some("hello "));
        assert!(input.backspace());
        assert_eq!(input.text(), Some("hello"));

        input.cancel();
        input.open();
        for _ in 0..(MAX_CHAT_LEN + 10) {
            input.push_char('a');
        }
        assert_eq!(input.text().map(str::len), Some(MAX_CHAT_LEN));

        let full = "a".repeat(MAX_CHAT_LEN);
        assert_eq!(input.submit().as_deref(), Some(full.as_str()));
        assert!(!input.is_active());
        assert_eq!(input.submit(), None);

        input.open();
        input.push_char(' ');
        assert_eq!(input.submit(), None, "blank chat is not sent");
        input.open();
        input.push_char('x');
        input.cancel();
        assert!(!input.is_active());
    }

    #[test]
    fn avatar_composition_is_stable_and_yaw_rotates_legs() {
        let one = AvatarState {
            id: 7,
            name: "alice".to_string(),
            pos: [2.5, 64.0, -3.5],
            yaw: 0.0,
        };
        let mut two = one.clone();
        two.id = 8;
        two.name = "bob".to_string();
        two.yaw = std::f32::consts::FRAC_PI_2;

        let instances = avatar_instances(&[one.clone(), two.clone()]);
        assert_eq!(instances.len(), 2 * AVATAR_CUBES);
        for instance in &instances {
            assert!(!instance.textured);
            assert_eq!(instance.layer, 0);
        }
        let first = &instances[..AVATAR_CUBES];
        assert_eq!(first[0].scale, 0.5, "head cube");
        assert!((first[0].position[1] - 65.0).abs() < 1e-5);
        assert_eq!(first[1].scale, 0.6, "torso cube");
        assert!((first[1].position[1] - 64.4).abs() < 1e-5);
        assert_eq!(first[2].scale, 0.4, "left leg");
        assert_eq!(first[3].scale, 0.4, "right leg");
        assert!((first[0].position[0] - 2.5).abs() < 1e-5, "yaw 0 faces -Z");
        assert!((first[0].position[2] - (-3.56)).abs() < 1e-5);
        assert!((first[2].position[0] - 2.35).abs() < 1e-5);
        assert!((first[3].position[0] - 2.65).abs() < 1e-5);
        assert_eq!(first[0].yaw, 0.0);

        let second = &instances[AVATAR_CUBES..];
        assert!((second[0].position[0] - 2.44).abs() < 1e-5, "head faces -X at yaw pi/2");
        assert!((second[2].position[2] - (-3.35)).abs() < 1e-5, "legs rotate with yaw");
        assert!((second[3].position[2] - (-3.65)).abs() < 1e-5);
        assert_eq!(second[0].yaw, std::f32::consts::FRAC_PI_2);

        assert_ne!(avatar_color(one.id, &one.name), avatar_color(two.id, &two.name));
        assert_eq!(avatar_color(one.id, &one.name), avatar_color(one.id, &one.name));
        let reordered = avatar_instances(&[two, one]);
        assert_eq!(reordered[0].tint, instances[AVATAR_CUBES].tint);
        assert_eq!(reordered[AVATAR_CUBES].tint, instances[0].tint);
        for tint in [instances[0].tint, instances[1].tint, instances[2].tint] {
            for channel in tint {
                assert!((0.0..=1.0).contains(&channel));
            }
        }
    }

    #[test]
    fn remote_player_interpolates_between_two_snapshots() {
        let start = Instant::now();
        let snapshot = PlayerSnapshot {
            id: 3,
            name: "bob".to_string(),
            pos: [0.0, 64.0, 0.0],
            yaw: 3.0,
            pitch: 0.2,
        };
        let mut player = RemotePlayer::new(&snapshot, start);
        assert_eq!(player.interpolated(start).0, [0.0, 64.0, 0.0]);
        player.update([2.0, 66.0, -2.0], -3.0, 0.4, start);
        let (halfway, yaw, pitch) = player.interpolated(start + Duration::from_millis(50));
        assert!((halfway[0] - 1.0).abs() < 1e-6);
        assert!((halfway[1] - 65.0).abs() < 1e-6);
        assert!((halfway[2] + 1.0).abs() < 1e-6);
        // 3.0 -> -3.0 is a short +0.283 rad arc across pi, not -6 rad.
        let expected = 3.0 + (std::f32::consts::TAU - 6.0) * 0.5;
        assert!((yaw - expected).abs() < 1e-5, "yaw {yaw} expected {expected}");
        assert!((pitch - 0.4).abs() < 1e-6);
        let (arrived, ..) = player.interpolated(start + Duration::from_secs_f32(INTERPOLATION_SECONDS * 2.0));
        assert_eq!(arrived, [2.0, 66.0, -2.0]);
        player.update([f64::NAN, 0.0, 0.0], 0.0, 0.0, start);
        assert_eq!(
            player
                .interpolated(start + Duration::from_secs_f32(INTERPOLATION_SECONDS * 4.0))
                .0,
            [2.0, 66.0, -2.0]
        );
    }

    fn hello_message() -> ServerMessage {
        let registry = Registry::base();
        ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            world_name: "unit".to_string(),
            seed: 42,
            spawn: [0.5, 70.0, 0.5],
            content_fingerprint: content_fingerprint(&registry),
            tick: 100,
            day_ticks: 24_000,
            max_players: 8,
            motd: "hello".to_string(),
            blocks: ob_protocol::block_manifest(&registry),
        }
    }

    #[test]
    fn handshake_hello_validation_rejects_mismatches() {
        let registry = Registry::base();
        let info = accept_hello(&hello_message(), &registry).expect("valid hello");
        assert_eq!(info.world_name, "unit");
        assert_eq!(info.seed, 42);
        assert_eq!(info.spawn, WorldVec::new(0.5, 70.0, 0.5));
        assert_eq!(info.max_players, 8);

        let mut wrong_protocol = hello_message();
        if let ServerMessage::Hello { protocol, .. } = &mut wrong_protocol {
            *protocol = PROTOCOL_VERSION + 1;
        }
        assert!(matches!(
            accept_hello(&wrong_protocol, &registry),
            Err(NetError::ProtocolMismatch { .. })
        ));

        let mut wrong_fingerprint = hello_message();
        if let ServerMessage::Hello {
            content_fingerprint: fingerprint,
            ..
        } = &mut wrong_fingerprint
        {
            *fingerprint ^= 1;
        }
        assert!(matches!(
            accept_hello(&wrong_fingerprint, &registry),
            Err(NetError::ContentMismatch { manifest: false, .. })
        ));

        let mut wrong_manifest = hello_message();
        if let ServerMessage::Hello { blocks, .. } = &mut wrong_manifest {
            blocks[0].name = "openblock:not_air".to_string();
        }
        assert!(matches!(
            accept_hello(&wrong_manifest, &registry),
            Err(NetError::ContentMismatch { manifest: true, .. })
        ));

        assert!(matches!(
            accept_hello(
                &ServerMessage::Disconnect {
                    reason: "server_full".to_string()
                },
                &registry
            ),
            Err(NetError::Handshake(_))
        ));
    }

    #[test]
    fn client_rate_limit_values_match_the_server_contract() {
        let start = Instant::now();
        let mut moves = RateLimit::new(30.0, 30);
        for _ in 0..30 {
            assert!(moves.allow(start), "30 moves/s burst");
        }
        assert!(!moves.allow(start), "the burst is exhausted");
        assert!(moves.allow(start + Duration::from_secs(1)), "moves refill");

        let mut edits = RateLimit::new(20.0, 20);
        for _ in 0..20 {
            assert!(edits.allow(start));
        }
        assert!(!edits.allow(start));

        let mut chat = RateLimit::new(0.5, 5);
        for _ in 0..5 {
            assert!(chat.allow(start), "5 chat lines per 10 s");
        }
        assert!(!chat.allow(start));
        assert!(!chat.allow(start + Duration::from_secs(1)), "chat refills slowly");
        assert!(chat.allow(start + Duration::from_secs(10)));

        let mut requests = RateLimit::new(16.0, 8);
        for _ in 0..8 {
            assert!(requests.allow(start));
        }
        assert!(!requests.allow(start));
    }

    #[test]
    fn state_transitions_and_friendly_reasons() {
        let mut state = NetState::Loading;
        assert!(state.is_connected());
        assert!(state.disconnect("server_full"));
        assert!(!state.is_connected());
        assert_eq!(state, NetState::Disconnected("server_full".to_string()));
        assert!(!state.disconnect("again"), "only the first transition is reported");
        assert_eq!(state.disconnect_reason(), Some("server_full"));

        assert_eq!(
            friendly_disconnect_reason("name_taken"),
            "That player name is already in use."
        );
        assert_eq!(
            friendly_disconnect_reason("server_full: at 8/8"),
            "The server is full. (at 8/8)"
        );
        assert_eq!(
            friendly_disconnect_reason("content_mismatch: packs differ"),
            "content_mismatch: packs differ"
        );
    }

    #[test]
    fn frame_reader_reassembles_frames_split_across_read_timeouts() {
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let writer = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let first = ob_protocol::encode_server_frame(&ServerMessage::Pong { id: 1 }).expect("frame one");
            let second = ob_protocol::encode_server_frame(&ServerMessage::Pong { id: 2 }).expect("frame two");
            socket.write_all(&first[..2]).expect("partial header");
            socket.flush().expect("flush");
            thread::sleep(Duration::from_millis(20));
            socket.write_all(&first[2..]).expect("rest of frame one");
            socket.write_all(&second[..5]).expect("partial frame two");
            socket.flush().expect("flush");
            thread::sleep(Duration::from_millis(20));
            socket.write_all(&second[5..]).expect("rest of frame two");
            socket.flush().expect("flush");
        });
        let stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(5)))
            .expect("short read timeout");
        let mut reader = FrameReader::new(stream);
        let mut decoded = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while decoded.len() < 2 {
            match reader.read_server_message(MAX_FRAME_LEN) {
                Ok(message) => decoded.push(message),
                Err(ProtocolError::Io(error))
                    if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                {
                    assert!(Instant::now() < deadline, "timed out reassembling frames");
                }
                Err(error) => panic!("frame reader failed: {error}"),
            }
        }
        assert_eq!(
            decoded,
            vec![ServerMessage::Pong { id: 1 }, ServerMessage::Pong { id: 2 }]
        );
        writer.join().expect("writer thread");
    }

    #[test]
    fn predictions_revert_latest_confirm_and_expire() {
        let start = Instant::now();
        let mut predictions = Predictions::default();
        let a = BlockPos::new(1, 2, 3);
        let b = BlockPos::new(4, 5, 6);
        predictions.remember(a, BlockId(1), BlockId::AIR, start);
        predictions.remember(b, BlockId(2), BlockId(3), start);
        assert_eq!(predictions.len(), 2);
        // Re-editing the same cell keeps the original previous block and moves
        // the cell to the back of the revert order.
        predictions.remember(a, BlockId(9), BlockId(4), start);
        assert_eq!(predictions.len(), 2);
        assert_eq!(predictions.entries[&a].previous, BlockId(1));
        let (pos, prediction) = predictions.revert_latest().expect("newest");
        assert_eq!(pos, a);
        assert_eq!((prediction.previous, prediction.placed), (BlockId(1), BlockId(4)));
        predictions.confirm(&b);
        assert_eq!(predictions.len(), 0);
        assert!(predictions.revert_latest().is_none());

        predictions.remember(a, BlockId::AIR, BlockId(2), start);
        let expired = predictions.expire(start + PREDICTION_TIMEOUT);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, a);
        assert_eq!(predictions.len(), 0);
    }

    #[test]
    fn predictions_are_bounded_to_the_oldest_cells() {
        let start = Instant::now();
        let mut predictions = Predictions::default();
        for index in 0..(MAX_PREDICTIONS + 5) {
            let pos = BlockPos::new(index as i32, 64, 0);
            predictions.remember(pos, BlockId::AIR, BlockId(1), start);
        }
        assert_eq!(predictions.len(), MAX_PREDICTIONS);
        assert!(predictions.entries.keys().all(|pos| pos.x >= 5));
        predictions.clear();
        assert_eq!(predictions.len(), 0);
    }

    #[test]
    fn endpoint_resolution_rejects_empty_and_bad_ports() {
        assert!(matches!(resolve_endpoint(""), Err(NetError::InvalidEndpoint(_))));
        assert!(resolve_endpoint("127.0.0.1:not-a-port").is_err());
        let loopback = resolve_endpoint("127.0.0.1:25565").expect("valid loopback endpoint");
        assert_eq!(loopback[0].port(), 25565);
    }
}

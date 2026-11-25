//! The interim dedicated-server network layer: plain TCP with the JSON protocol.
//!
//! [`DedicatedServer::start`] wraps a [`Host`] in a set of threads:
//!
//! * an accept thread that performs the handshake and then spawns
//!   per-connection reader/writer threads,
//! * one tick thread at 20 Hz that owns the authoritative [`Host`], applies
//!   validated client intents, streams chunks, replicates entities, advances
//!   container windows and saves,
//! * reader threads (bounded frame size, per-connection inbound rate limit) and
//!   writer threads (bounded outbound queue; slow clients are disconnected).
//!
//! The shared [`Host`] is behind one mutex. No network thread holds that lock
//! while doing blocking socket I/O, so a slow client cannot stall the tick.
//! Query methods on [`DedicatedServer`] lock briefly and are intended for tests,
//! the console and future admin surfaces.
//!
//! # Protocol v2 survival
//!
//! Network players are server-authoritative for survival state: vitals,
//! inventory, equipment, block actions, combat, item pickup, death/respawn and
//! furnace/chest windows are all decided by the server. Movement remains
//! client-authoritative (bounds/speed/teleport validation plus fall-damage
//! tracking only), because network-player physics/reconciliation is a later
//! milestone. See [`crate::net_players`] and
//! `docs/spec/18-networking-multiplayer.md` §3.13 for the state machine and
//! known limitations.
//!
//! # Security model
//!
//! This transport has **no authentication, encryption or QUIC**. It is meant
//! for loopback, LAN and trusted self-hosting behind a firewall. The server
//! validates every intent it accepts (protocol version, message size and rate,
//! slot bounds, reach, block/item ids, container distance, window ownership,
//! stack conservation and entity ids), but movement is client-authoritative and
//! a name is a label, not a credential.
//!
//! # Configuration file
//!
//! [`parse_server_config`] accepts a deliberately small flat TOML subset:
//! `key = "string"`, integer literals with `_`, and a string array for
//! `whitelist`. Tables, nested keys and other TOML features are rejected. The
//! binary maps these values into the host configuration and [`NetConfig`]; CLI
//! flags override the file.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ob_content::{BlockId, MobKind};
use ob_math::{BlockPos, ChunkPos, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec};
use ob_protocol::{
    BlockDescriptor, ClientMessage, ContainerKind, DespawnReason, EQUIPMENT_SLOTS, EntityKind, EntitySnapshot,
    EntityStateEntry, FurnaceProgress, HOTBAR_SLOTS, ItemStackDto, MAX_FRAME_LEN, PROTOCOL_VERSION, PlayerSnapshot,
    RateLimit, SLOT_WINDOW_START, ServerMessage, block_manifest, content_fingerprint, decode_payload,
    encode_chunk_checked, encode_server_frame, read_frame, sanitise_motd, validate_chat_text, validate_player_name,
};
use ob_sim::REACH;

use crate::net_players::{ENTITY_INTEREST_RADIUS, NetworkWindowRef, ServerPlayer, stack_dto, wire_entity_id};
use crate::{GameMode, Host, HostError, ItemStack};

/// Default TCP port of the interim server.
pub const DEFAULT_PORT: u16 = 25565;
/// Default maximum number of simultaneous players.
pub const DEFAULT_MAX_PLAYERS: u16 = 8;
/// Default MOTD advertised in the handshake.
pub const DEFAULT_MOTD: &str = "A self-hosted OpenBlock world";
/// Default horizontal interest radius in chunks.
pub const DEFAULT_INTEREST_RADIUS_CHUNKS: i32 = 6;
/// Hard upper bound on a client's requested/streamed interest radius.
pub const MAX_INTEREST_RADIUS_CHUNKS: i32 = 16;
/// Default number of chunk columns generated/sent per player per tick.
pub const DEFAULT_CHUNKS_PER_TICK: usize = 4;
/// Default autosave interval in seconds (`0` disables periodic autosave).
pub const DEFAULT_SAVE_INTERVAL_SECONDS: u64 = 60;
/// Maximum frames buffered for one client before it is considered too slow.
pub const MAX_QUEUED_FRAMES: usize = 256;
/// Maximum concurrent handshakes (a small bound against connection floods).
pub const MAX_CONCURRENT_HANDSHAKES: usize = 32;
/// Default network game mode (survival in protocol v2).
pub const DEFAULT_NETWORK_GAME_MODE: GameMode = GameMode::Survival;

/// One simulation tick (20 Hz).
const TICK_INTERVAL: Duration = Duration::from_millis(50);
/// Timeout for the Hello exchange.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for a single socket write.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// Idle read timeout; clients should send a Ping when otherwise idle.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
/// A move farther than this from the sender's last position is rejected.
const MAX_MOVE_DISTANCE: f64 = 64.0;
/// Horizontal world border used to clamp client-authoritative positions.
const MAX_WORLD_XZ: f64 = 30_000_000.0;
/// Lowest accepted feet Y (a little below the world floor).
const MIN_PLAYER_Y: f64 = WORLD_MIN_Y as f64 - 32.0;
/// Highest accepted feet Y (above the build limit for creative flight).
const MAX_PLAYER_Y: f64 = WORLD_MAX_Y as f64 + 256.0;
/// Maximum accepted block-edit distance from the sender's last position.
const MAX_EDIT_DISTANCE: f64 = REACH + 2.0;
/// Maximum distance from a player to a furnace/chest they open or keep open.
const MAX_CONTAINER_DISTANCE: f64 = 6.0;
/// Inbound frame rate cap applied before the tick loop sees messages.
const INBOUND_FRAMES_PER_SECOND: f64 = 120.0;
/// Inbound burst allowance.
const INBOUND_BURST: u32 = 240;
/// Move intents accepted per second per client.
const MOVES_PER_SECOND: f64 = 30.0;
/// Edit intents accepted per second per client.
const EDITS_PER_SECOND: f64 = 20.0;
/// Chat lines accepted per second per client (spec: 5 per 10 s).
const CHAT_PER_SECOND: f64 = 0.5;
/// Chunk requests accepted per second per client.
const CHUNK_REQUESTS_PER_SECOND: f64 = 16.0;
/// Block-use intents accepted per second per client.
const USE_BLOCKS_PER_SECOND: f64 = 5.0;
/// Attack intents accepted per second per client.
const ATTACKS_PER_SECOND: f64 = 10.0;
/// Inventory transactions accepted per second per client.
const INVENTORY_MOVES_PER_SECOND: f64 = 20.0;
/// Eat intents accepted per second per client.
const EAT_PER_SECOND: f64 = 4.0;
/// Equip intents accepted per second per client.
const EQUIPS_PER_SECOND: f64 = 5.0;
/// Slot selections accepted per second per client.
const SELECTS_PER_SECOND: f64 = 20.0;
/// Respawn intents accepted per second per client.
const RESPAWNS_PER_SECOND: f64 = 1.0;
/// Container-close intents accepted per second per client.
const CONTAINER_CLOSES_PER_SECOND: f64 = 10.0;
/// Entity spawn/state/despawn replication cadence (every N ticks = 10 Hz).
const ENTITY_SYNC_INTERVAL_TICKS: u64 = 2;
/// Periodic self-vitals cadence.
const VITALS_SYNC_INTERVAL_TICKS: u64 = 10;
/// Periodic full inventory resync when nothing changed (1 Hz at 20 Hz ticks).
const INVENTORY_RESYNC_INTERVAL_TICKS: u64 = 20;
/// Furnace progress windows update at 5 Hz.
const WINDOW_UPDATE_INTERVAL_TICKS: u64 = 4;
/// Maximum length of a disconnect reason string.
const MAX_REASON_LEN: usize = 200;
/// Maximum allowed horizontal movement per accepted Move, in metres per
/// second of elapsed time (generous to tolerate jitter; not anti-cheat grade).
const MOVE_ALLOWANCE_PER_SECOND: f64 = 20.0;
/// Minimum horizontal movement allowance for one Move message, in metres.
const MOVE_MIN_ALLOWANCE: f64 = 2.0;
/// Maximum allowed vertical movement per accepted Move, in metres per second
/// of elapsed time. Larger than horizontal so a legitimate terminal-velocity
/// fall is not mistaken for a teleport.
const FALL_ALLOWANCE_PER_SECOND: f64 = 100.0;
/// Minimum vertical movement allowance for one Move message, in metres.
const FALL_MIN_ALLOWANCE: f64 = 5.0;
/// Tick durations kept for the rolling p50/p95/p99 window (60 s at 20 Hz).
const TICK_HISTORY_SAMPLES: usize = 1200;
/// A tick longer than this misses the 20 Hz deadline (50 ms period).
const TICK_OVERRUN: Duration = Duration::from_millis(50);
/// A tick longer than this misses the 1.0 p99 tick envelope (PERF-TICK-009).
const TICK_ENVELOPE: Duration = Duration::from_millis(35);

/// Runtime configuration for the dedicated network layer.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Address to bind (`127.0.0.1:0` selects an ephemeral test port).
    pub listen: SocketAddr,
    /// Maximum number of simultaneously connected players.
    pub max_players: u16,
    /// MOTD sent in the handshake (sanitised and truncated to 128 bytes).
    pub motd: String,
    /// Per-client interest radius in chunks (`1..=16`).
    pub interest_radius_chunks: i32,
    /// Chunk columns generated and sent per client per tick (`1..=64`).
    pub chunks_per_tick: usize,
    /// Autosave interval in seconds; `0` disables periodic autosave.
    pub save_interval_seconds: u64,
    /// Case-insensitive player-name allow list; empty means open.
    pub whitelist: Vec<String>,
    /// Game mode for players joining without a saved record.
    pub game_mode: GameMode,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            max_players: DEFAULT_MAX_PLAYERS,
            motd: DEFAULT_MOTD.to_string(),
            interest_radius_chunks: DEFAULT_INTEREST_RADIUS_CHUNKS,
            chunks_per_tick: DEFAULT_CHUNKS_PER_TICK,
            save_interval_seconds: DEFAULT_SAVE_INTERVAL_SECONDS,
            whitelist: Vec::new(),
            game_mode: DEFAULT_NETWORK_GAME_MODE,
        }
    }
}

impl NetConfig {
    /// Validates limits and normalises the MOTD/whitelist.
    ///
    /// Returns a human-readable error for the binary's exit-2 path. The
    /// network layer never panics on invalid configuration.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=500).contains(&self.max_players) {
            return Err("max_players must be in 1..=500".to_string());
        }
        if !(1..=MAX_INTEREST_RADIUS_CHUNKS).contains(&self.interest_radius_chunks) {
            return Err(format!(
                "interest_radius_chunks must be in 1..={MAX_INTEREST_RADIUS_CHUNKS}"
            ));
        }
        if !(1..=64).contains(&self.chunks_per_tick) {
            return Err("chunks_per_tick must be in 1..=64".to_string());
        }
        if self.save_interval_seconds > 86_400 {
            return Err("save_interval_seconds must be at most 86400".to_string());
        }
        for name in &self.whitelist {
            validate_player_name(name).map_err(|error| format!("invalid whitelist entry '{name}': {error}"))?;
        }
        Ok(())
    }

    /// Applies validation and safe normalisation, logging a warning on issues.
    fn sanitised(mut self) -> Self {
        if self.max_players == 0 || self.max_players > 500 {
            log::warn!("max_players {} is out of range; clamping", self.max_players);
            self.max_players = self.max_players.clamp(1, 500);
        }
        if !(1..=MAX_INTEREST_RADIUS_CHUNKS).contains(&self.interest_radius_chunks) {
            log::warn!(
                "interest radius {} is out of range; clamping to 1..={MAX_INTEREST_RADIUS_CHUNKS}",
                self.interest_radius_chunks
            );
            self.interest_radius_chunks = self.interest_radius_chunks.clamp(1, MAX_INTEREST_RADIUS_CHUNKS);
        }
        if self.chunks_per_tick == 0 || self.chunks_per_tick > 64 {
            log::warn!("chunks_per_tick {} is out of range; clamping", self.chunks_per_tick);
            self.chunks_per_tick = self.chunks_per_tick.clamp(1, 64);
        }
        if self.save_interval_seconds > 86_400 {
            self.save_interval_seconds = 86_400;
        }
        self.motd = sanitise_motd(&self.motd);
        self.whitelist.retain(|name| {
            if validate_player_name(name).is_ok() {
                true
            } else {
                log::warn!("ignoring invalid whitelist entry '{name}'");
                false
            }
        });
        self
    }
}

/// Lock-free counters and a rolling tick-duration window for `--metrics-json`.
///
/// Every field is updated with relaxed atomics so the tick and socket threads
/// never take another lock for observability. The single tick thread is the
/// only writer of [`Metrics::tick_micros`], so a snapshot's window can only be
/// short of the newest tick, never internally reordered.
struct Metrics {
    tick_micros: [AtomicU64; TICK_HISTORY_SAMPLES],
    tick_sequence: AtomicU64,
    tick_overruns: AtomicU64,
    tick_over_envelope: AtomicU64,
    chunks_queued: AtomicU64,
    chunk_bytes_queued: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    moves: AtomicU64,
    edits_attempted: AtomicU64,
    chats: AtomicU64,
    pings: AtomicU64,
    chunk_requests: AtomicU64,
    actions_rejected: AtomicU64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            tick_micros: std::array::from_fn(|_| AtomicU64::new(0)),
            tick_sequence: AtomicU64::new(0),
            tick_overruns: AtomicU64::new(0),
            tick_over_envelope: AtomicU64::new(0),
            chunks_queued: AtomicU64::new(0),
            chunk_bytes_queued: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            moves: AtomicU64::new(0),
            edits_attempted: AtomicU64::new(0),
            chats: AtomicU64::new(0),
            pings: AtomicU64::new(0),
            chunk_requests: AtomicU64::new(0),
            actions_rejected: AtomicU64::new(0),
        }
    }

    /// Records one completed tick duration into the rolling window.
    fn record_tick(&self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        let index = (self.tick_sequence.load(Ordering::Relaxed) as usize) % TICK_HISTORY_SAMPLES;
        self.tick_micros[index].store(micros, Ordering::Relaxed);
        self.tick_sequence.fetch_add(1, Ordering::Relaxed);
        if duration > TICK_OVERRUN {
            self.tick_overruns.fetch_add(1, Ordering::Relaxed);
        }
        if duration > TICK_ENVELOPE {
            self.tick_over_envelope.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counts one inbound message of a given type.
    fn count(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Nearest-rank p50/p95/p99 and max over the rolling window, in ms.
    fn tick_stats(&self) -> (usize, f64, f64, f64, f64) {
        let written = self.tick_sequence.load(Ordering::Relaxed);
        let count = written.min(TICK_HISTORY_SAMPLES as u64) as usize;
        if count == 0 {
            return (0, 0.0, 0.0, 0.0, 0.0);
        }
        let mut samples: Vec<u64> = (0..count)
            .map(|index| self.tick_micros[index].load(Ordering::Relaxed))
            .collect();
        samples.sort_unstable();
        (
            count,
            percentile_micros(&samples, 50.0),
            percentile_micros(&samples, 95.0),
            percentile_micros(&samples, 99.0),
            micros_to_ms(samples[count - 1]),
        )
    }
}

/// Nearest-rank percentile of an ascending microsecond series, in milliseconds.
fn percentile_micros(sorted: &[u64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (percentile.clamp(0.0, 100.0) / 100.0 * sorted.len() as f64).ceil() as usize;
    let index = rank.clamp(1, sorted.len()) - 1;
    micros_to_ms(sorted[index])
}

/// Converts integer microseconds to a millisecond value for reports.
fn micros_to_ms(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

/// A cheap observability snapshot of one running dedicated server.
#[derive(Debug, Clone)]
pub struct ServerMetrics {
    /// Seconds since the network layer started its tick thread.
    pub uptime_seconds: f64,
    /// Bound TCP address, useful when the caller requested port 0.
    pub listen: String,
    /// World directory name.
    pub world_name: String,
    /// World seed.
    pub seed: u64,
    /// Default game mode for new network players.
    pub game_mode: &'static str,
    /// Configured player cap.
    pub max_players: u16,
    /// Configured per-client interest radius in chunks.
    pub interest_radius_chunks: i32,
    /// Chunk columns streamed per client per tick.
    pub chunks_per_tick: usize,
    /// Autosave interval in seconds (`0` disables it).
    pub save_interval_seconds: u64,
    /// Completed server ticks.
    pub tick_count: u64,
    /// Most recent one-second TPS measurement.
    pub tps: f32,
    /// Ticks currently in the rolling window.
    pub tick_samples: usize,
    /// Tick p50 over the rolling window, in milliseconds.
    pub tick_p50_ms: f64,
    /// Tick p95 over the rolling window, in milliseconds.
    pub tick_p95_ms: f64,
    /// Tick p99 over the rolling window, in milliseconds.
    pub tick_p99_ms: f64,
    /// Longest tick over the rolling window, in milliseconds.
    pub tick_max_ms: f64,
    /// Ticks above the 50 ms period since start.
    pub tick_overruns: u64,
    /// Ticks above the 35 ms 1.0 envelope since start.
    pub tick_over_envelope: u64,
    /// Connected players at snapshot time.
    pub players_connected: usize,
    /// Chunk frames queued to per-client writers since start.
    pub chunks_queued: u64,
    /// Encoded bytes in those chunk frames.
    pub chunk_bytes_queued: u64,
    /// Bytes received on player sockets (framing included).
    pub bytes_in: u64,
    /// Bytes written to player sockets (framing included).
    pub bytes_out: u64,
    /// Move intents received.
    pub moves: u64,
    /// Block-edit intents received (accepted and rejected).
    pub edits_attempted: u64,
    /// Accepted block edits.
    pub edits_accepted: u64,
    /// Rejected block edits.
    pub edits_rejected: u64,
    /// Chat lines received.
    pub chats: u64,
    /// Ping intents received.
    pub pings: u64,
    /// Chunk requests received.
    pub chunk_requests: u64,
    /// Action rejections of every kind sent to clients.
    pub actions_rejected: u64,
    /// Loaded mobs at snapshot time.
    pub mobs: usize,
    /// Dropped item entities at snapshot time.
    pub item_entities: usize,
    /// Resident world columns at snapshot time.
    pub loaded_columns: usize,
    /// Process RSS from `/proc/self/status` on Linux.
    pub rss_kib: Option<u64>,
    /// Process thread count from `/proc/self/status` on Linux.
    pub threads: Option<u64>,
    /// Open file descriptors from `/proc/self/fd` on Linux.
    pub open_fds: Option<u64>,
}

/// Reads RSS, thread count and open descriptors from procfs when available.
fn process_stats() -> (Option<u64>, Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok();
        let field = |name: &str| {
            status.as_deref().and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix(name))
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|value| value.parse::<u64>().ok())
            })
        };
        let rss = field("VmRSS:");
        let threads = field("Threads:");
        let fds = std::fs::read_dir("/proc/self/fd")
            .ok()
            .map(|entries| entries.count() as u64);
        (rss, threads, fds)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (None, None, None)
    }
}

/// A running dedicated server.
///
/// The server owns the [`Host`] and shuts it down gracefully on [`stop`].
/// Dropping the handle requests shutdown but does not wait or save; call
/// [`DedicatedServer::stop`] for a clean, deterministic exit.
///
/// [`stop`]: DedicatedServer::stop
pub struct DedicatedServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    tick_join: Option<JoinHandle<()>>,
    accept_join: Option<JoinHandle<()>>,
}

struct Shared {
    core: Mutex<Core>,
    events: Sender<ServerEvent>,
    shutdown: AtomicBool,
    handshakes: AtomicUsize,
    metrics: Arc<Metrics>,
}

enum ServerEvent {
    Message { id: u64, message: ClientMessage },
    Closed { id: u64, reason: Option<String> },
    WriterFailed { id: u64 },
}

struct Core {
    host: Host,
    config: NetConfig,
    players: BTreeMap<u64, NetPlayer>,
    next_id: u64,
    next_window_id: u32,
    tick_count: u64,
    started: Instant,
    metrics: Arc<Metrics>,
    dirty: bool,
    last_save: Instant,
    edits_accepted: u64,
    edits_rejected: u64,
    tps: f32,
    fingerprint: u64,
    blocks: Vec<BlockDescriptor>,
    world_name: String,
    seed: u64,
    spawn: [f64; 3],
    shutting_down: bool,
}

struct NetPlayer {
    id: u64,
    name: String,
    pos: WorldVec,
    yaw: f32,
    pitch: f32,
    sender: SyncSender<Vec<u8>>,
    stream: TcpStream,
    sent_chunks: HashSet<ChunkPos>,
    last_move: Instant,
    move_limit: RateLimit,
    edit_limit: RateLimit,
    use_limit: RateLimit,
    attack_limit: RateLimit,
    inventory_limit: RateLimit,
    eat_limit: RateLimit,
    equip_limit: RateLimit,
    select_limit: RateLimit,
    respawn_limit: RateLimit,
    close_limit: RateLimit,
    chat_limit: RateLimit,
    chunk_limit: RateLimit,
    ping_limit: RateLimit,
    window: Option<OpenWindow>,
    known_entities: BTreeMap<u64, EntityView>,
}

impl NetPlayer {
    fn snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            id: self.id,
            name: self.name.clone(),
            pos: [self.pos.x, self.pos.y, self.pos.z],
            yaw: self.yaw,
            pitch: self.pitch,
        }
    }
}

/// One open furnace/chest window owned by a connection.
#[derive(Debug, Clone)]
struct OpenWindow {
    id: u32,
    pos: BlockPos,
    kind: ContainerKind,
    revision: u64,
    slots: Vec<Option<ItemStack>>,
    furnace: Option<FurnaceProgress>,
}

impl OpenWindow {
    fn window_ref(&self) -> NetworkWindowRef {
        NetworkWindowRef {
            pos: self.pos,
            slots: self.slots.len(),
            furnace: self.kind == ContainerKind::Furnace,
        }
    }
}

impl DedicatedServer {
    /// Binds `config.listen`, starts the tick and accept threads, and returns
    /// the running server. The host must already be opened by the caller.
    pub fn start(host: Host, config: NetConfig) -> io::Result<Self> {
        config
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let config = config.sanitised();
        let listener = TcpListener::bind(config.listen)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;

        let blocks = block_manifest(host.registry());
        let fingerprint = content_fingerprint(host.registry());
        let world_name = host.config().world_name.clone();
        let seed = host.world().seed();
        let spawn = host.spawn();
        log::info!(
            "starting TCP server on {addr}: world '{world_name}' seed {seed}, max {} players, interest {} chunks, default mode {}",
            config.max_players,
            config.interest_radius_chunks,
            config.game_mode.name()
        );

        let metrics = Arc::new(Metrics::new());
        let core = Core {
            host,
            config,
            players: BTreeMap::new(),
            next_id: 1,
            next_window_id: 1,
            tick_count: 0,
            started: Instant::now(),
            metrics: Arc::clone(&metrics),
            dirty: false,
            last_save: Instant::now(),
            edits_accepted: 0,
            edits_rejected: 0,
            tps: 0.0,
            fingerprint,
            blocks,
            world_name,
            seed,
            spawn: [spawn.x, spawn.y, spawn.z],
            shutting_down: false,
        };
        let (events, receiver) = mpsc::channel();
        let shared = Arc::new(Shared {
            core: Mutex::new(core),
            events,
            shutdown: AtomicBool::new(false),
            handshakes: AtomicUsize::new(0),
            metrics,
        });

        let tick_shared = Arc::clone(&shared);
        let tick_join = thread::Builder::new()
            .name("ob-net-tick".to_string())
            .spawn(move || run_tick_loop(tick_shared, receiver))?;

        let accept_shared = Arc::clone(&shared);
        let accept_join = match thread::Builder::new()
            .name("ob-net-accept".to_string())
            .spawn(move || run_accept_loop(accept_shared, listener))
        {
            Ok(join) => join,
            Err(error) => {
                shared.shutdown.store(true, Ordering::SeqCst);
                let _ = tick_join.join();
                return Err(error);
            }
        };

        Ok(Self {
            addr,
            shared,
            tick_join: Some(tick_join),
            accept_join: Some(accept_join),
        })
    }

    /// The bound address (useful when the caller requested port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Number of connected players.
    pub fn player_count(&self) -> usize {
        lock(&self.shared.core).players.len()
    }

    /// Configured player cap.
    pub fn max_players(&self) -> u16 {
        lock(&self.shared.core).config.max_players
    }

    /// Default network game mode from the configuration.
    pub fn game_mode(&self) -> GameMode {
        lock(&self.shared.core).config.game_mode
    }

    /// Configured per-client interest radius in chunks.
    pub fn interest_radius_chunks(&self) -> i32 {
        lock(&self.shared.core).config.interest_radius_chunks
    }

    /// Snapshot of every connected player, in ascending connection-id order.
    pub fn players(&self) -> Vec<PlayerSnapshot> {
        lock(&self.shared.core)
            .players
            .values()
            .map(NetPlayer::snapshot)
            .collect()
    }

    /// Cloned authoritative network-player states, in ascending id order.
    ///
    /// Admin/test surface: live game logic does not use it.
    pub fn server_players(&self) -> Vec<ServerPlayer> {
        lock(&self.shared.core)
            .host
            .network_players()
            .values()
            .cloned()
            .collect()
    }

    /// One cloned authoritative network player by connection id.
    pub fn server_player(&self, id: u64) -> Option<ServerPlayer> {
        lock(&self.shared.core).host.network_player(id).cloned()
    }

    /// One persisted offline network player by case-insensitive name.
    pub fn saved_server_player(&self, name: &str) -> Option<ServerPlayer> {
        lock(&self.shared.core).host.saved_network_player(name).cloned()
    }

    /// Spawns one mob directly (admin/test surface).
    pub fn spawn_mob(&self, kind: MobKind, pos: [f64; 3]) -> Option<u64> {
        let pos = WorldVec::new(pos[0], pos[1], pos[2]);
        let mut core = lock(&self.shared.core);
        let id = core.host.spawn_mob(kind, pos)?;
        core.dirty = true;
        Some(id)
    }

    /// Gives items to a network player (admin/test surface).
    ///
    /// Returns `false` when the player is unknown, the item name is unknown or
    /// nothing fitted.
    pub fn give_network_item(&self, id: u64, item: &str, count: u8) -> bool {
        let mut core = lock(&self.shared.core);
        let Some(item) = core.host.registry().item_id(item) else {
            return false;
        };
        let overflow = core.host.give_network_item(id, item, count);
        if overflow >= count {
            false
        } else {
            core.dirty = true;
            true
        }
    }

    /// Sets a network player's health/hunger (admin/test surface).
    pub fn set_network_player_vitals(&self, id: u64, health: f32, hunger: f32) -> bool {
        let mut core = lock(&self.shared.core);
        core.dirty = true;
        core.host.set_network_player_vitals(id, health, hunger)
    }

    /// Applies damage to a network player (admin/test surface).
    ///
    /// The resulting `DamageEvent`/`Death` is delivered by the next tick.
    pub fn damage_network_player(&self, id: u64, amount: f32) -> bool {
        let mut core = lock(&self.shared.core);
        let damaged = core.host.damage_network_player(id, amount, "admin").is_some();
        if damaged {
            core.dirty = true;
        }
        damaged
    }

    /// A loaded block, or `None` when the position is out of the world or its
    /// chunk is not resident. Intended for tests/admin surfaces.
    pub fn block_at(&self, pos: BlockPos) -> Option<BlockId> {
        if !pos.in_world() {
            return None;
        }
        let core = lock(&self.shared.core);
        let chunk = core.host.world().chunk(pos.chunk())?;
        Some(chunk.get_block(pos))
    }

    /// The number of completed server ticks.
    pub fn tick_count(&self) -> u64 {
        lock(&self.shared.core).tick_count
    }

    /// Most recent one-second TPS measurement.
    pub fn tps(&self) -> f32 {
        lock(&self.shared.core).tps
    }

    /// Number of accepted and rejected block edits since start (accepted,
    /// rejected), for logs and tests.
    pub fn edit_counts(&self) -> (u64, u64) {
        let core = lock(&self.shared.core);
        (core.edits_accepted, core.edits_rejected)
    }

    /// A consistent snapshot of the server's observability counters.
    ///
    /// Locks the core for the duration of a few reads and a sort of at most
    /// 1,200 integers; it is meant for a 1-5 s metrics
    /// writer, not the tick loop.
    pub fn metrics(&self) -> ServerMetrics {
        let core = lock(&self.shared.core);
        let (tick_samples, tick_p50_ms, tick_p95_ms, tick_p99_ms, tick_max_ms) = core.metrics.tick_stats();
        let (rss_kib, threads, open_fds) = process_stats();
        ServerMetrics {
            uptime_seconds: core.started.elapsed().as_secs_f64(),
            listen: self.addr.to_string(),
            world_name: core.world_name.clone(),
            seed: core.seed,
            game_mode: core.config.game_mode.name(),
            max_players: core.config.max_players,
            interest_radius_chunks: core.config.interest_radius_chunks,
            chunks_per_tick: core.config.chunks_per_tick,
            save_interval_seconds: core.config.save_interval_seconds,
            tick_count: core.tick_count,
            tps: core.tps,
            tick_samples,
            tick_p50_ms,
            tick_p95_ms,
            tick_p99_ms,
            tick_max_ms,
            tick_overruns: core.metrics.tick_overruns.load(Ordering::Relaxed),
            tick_over_envelope: core.metrics.tick_over_envelope.load(Ordering::Relaxed),
            players_connected: core.players.len(),
            chunks_queued: core.metrics.chunks_queued.load(Ordering::Relaxed),
            chunk_bytes_queued: core.metrics.chunk_bytes_queued.load(Ordering::Relaxed),
            bytes_in: core.metrics.bytes_in.load(Ordering::Relaxed),
            bytes_out: core.metrics.bytes_out.load(Ordering::Relaxed),
            moves: core.metrics.moves.load(Ordering::Relaxed),
            edits_attempted: core.metrics.edits_attempted.load(Ordering::Relaxed),
            edits_accepted: core.edits_accepted,
            edits_rejected: core.edits_rejected,
            chats: core.metrics.chats.load(Ordering::Relaxed),
            pings: core.metrics.pings.load(Ordering::Relaxed),
            chunk_requests: core.metrics.chunk_requests.load(Ordering::Relaxed),
            actions_rejected: core.metrics.actions_rejected.load(Ordering::Relaxed),
            mobs: core.host.mobs().len(),
            item_entities: core.host.item_entities().len(),
            loaded_columns: core.host.world().chunk_count(),
            rss_kib,
            threads,
            open_fds,
        }
    }

    /// Saves the world immediately. Atomic, like [`Host::save`].
    pub fn save(&self) -> Result<(), HostError> {
        lock(&self.shared.core).host.save()
    }

    /// Broadcasts a `server` chat line to every player.
    ///
    /// Returns `false` when the text is empty/too long or the server is
    /// shutting down. The message is not delivered to clients that already
    /// have a full outbound queue.
    pub fn broadcast_chat(&self, text: &str) -> bool {
        let Ok(text) = validate_chat_text(text) else {
            return false;
        };
        let mut core = lock(&self.shared.core);
        if core.shutting_down {
            return false;
        }
        core.broadcast(
            &ServerMessage::Chat {
                from: "server".to_string(),
                text,
            },
            None,
        );
        true
    }

    /// Kicks the player with the given connection id or (case-insensitive)
    /// name. Returns whether a player was removed.
    pub fn kick(&self, target: &str, reason: &str) -> bool {
        let reason = sanitise_reason(reason);
        let mut core = lock(&self.shared.core);
        let id = target
            .parse::<u64>()
            .ok()
            .and_then(|id| core.players.contains_key(&id).then_some(id))
            .or_else(|| {
                core.players
                    .values()
                    .find(|player| player.name.eq_ignore_ascii_case(target))
                    .map(|player| player.id)
            });
        let Some(id) = id else {
            return false;
        };
        core.disconnect(id, Some(reason), "kicked by admin");
        true
    }

    /// Requests a graceful shutdown without waiting. The tick thread stops,
    /// disconnects every client with `server_shutdown` and returns; call
    /// [`DedicatedServer::stop`] to join and save.
    pub fn request_shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
    }

    /// Stops the server, joins the tick and accept threads, flushes queued
    /// disconnect frames and saves the world.
    ///
    /// The save is performed once, by this method, so a successful return
    /// means the world is on disk. Client reader/writer threads are detached
    /// but have their sockets shut down by the tick thread and exit promptly.
    pub fn stop(mut self) -> Result<(), HostError> {
        self.request_shutdown();
        if let Some(join) = self.accept_join.take() {
            let _ = join.join();
        }
        if let Some(join) = self.tick_join.take() {
            let _ = join.join();
        }
        // Give writers a moment to flush their queued Disconnect frames before
        // the world save returns and the caller may close the process.
        thread::sleep(Duration::from_millis(100));
        lock(&self.shared.core).host.save()
    }
}

impl Drop for DedicatedServer {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
    }
}

/// Recovers a poisoned mutex instead of propagating a panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Accept loop: non-blocking accept with a short poll interval so shutdown is
/// prompt. Each accepted socket gets a dedicated handshake thread.
fn run_accept_loop(shared: Arc<Shared>, listener: TcpListener) {
    while !shared.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if shared.handshakes.load(Ordering::SeqCst) >= MAX_CONCURRENT_HANDSHAKES {
                    // Do not log the remote address by default (REQ-SEC-030).
                    log::warn!("rejecting a connection: too many concurrent handshakes");
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                shared.handshakes.fetch_add(1, Ordering::SeqCst);
                let handshake_shared = Arc::clone(&shared);
                let spawned = thread::Builder::new()
                    .name("ob-net-handshake".to_string())
                    .spawn(move || {
                        handshake(handshake_shared.clone(), stream);
                        handshake_shared.handshakes.fetch_sub(1, Ordering::SeqCst);
                    });
                if let Err(error) = spawned {
                    shared.handshakes.fetch_sub(1, Ordering::SeqCst);
                    log::warn!("could not spawn a handshake thread: {error}");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                log::warn!("tcp accept failed: {error}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Performs the Hello exchange and registers a player.
fn handshake(shared: Arc<Shared>, stream: TcpStream) {
    // Darwin (and some BSDs) inherit the listener's non-blocking flag on
    // accepted sockets, unlike Linux. Handshake and gameplay I/O assume a
    // blocking socket with explicit read/write timeouts; switch it back before
    // the first read or macOS clients would see an immediate WouldBlock and be
    // rejected as "timeout".
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(error) => {
            log::warn!("handshake failed: {error}");
            return;
        }
    };
    let payload = match read_frame(&mut reader, MAX_FRAME_LEN) {
        Ok(payload) => payload,
        Err(error) => {
            reject_handshake(&stream, &handshake_error_reason(&error));
            return;
        }
    };
    shared
        .metrics
        .bytes_in
        .fetch_add(payload.len() as u64 + 4, Ordering::Relaxed);
    let message: ClientMessage = match decode_payload(&payload, MAX_FRAME_LEN) {
        Ok(message) => message,
        Err(error) => {
            reject_handshake(&stream, &handshake_error_reason(&error));
            return;
        }
    };
    let (protocol, name) = match message {
        ClientMessage::Hello { protocol, name } => (protocol, name),
        _ => {
            reject_handshake(&stream, "protocol_error: expected hello");
            return;
        }
    };
    if protocol != PROTOCOL_VERSION {
        reject_handshake(
            &stream,
            &format!(
                "protocol_mismatch: server protocol {PROTOCOL_VERSION}, client sent {protocol}; version 1 clients must upgrade"
            ),
        );
        return;
    }
    if let Err(error) = validate_player_name(&name) {
        reject_handshake(&stream, &format!("invalid_name: {error}"));
        return;
    }

    let mut core = lock(&shared.core);
    if core.shutting_down {
        drop(core);
        reject_handshake(&stream, "server_shutdown");
        return;
    }
    if core.players.len() >= usize::from(core.config.max_players) {
        drop(core);
        reject_handshake(&stream, "server_full");
        return;
    }
    if core
        .players
        .values()
        .any(|player| player.name.eq_ignore_ascii_case(&name))
    {
        drop(core);
        reject_handshake(&stream, "name_taken");
        return;
    }
    if !core.config.whitelist.is_empty()
        && !core
            .config
            .whitelist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&name))
    {
        drop(core);
        reject_handshake(&stream, "not_whitelisted");
        return;
    }

    let id = core.next_id;
    core.next_id = core.next_id.saturating_add(1);
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(MAX_QUEUED_FRAMES);
    let writer_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(error) => {
            drop(core);
            log::warn!("handshake failed: {error}");
            return;
        }
    };
    let writer_shared = Arc::clone(&shared);
    let writer = thread::Builder::new()
        .name(format!("ob-net-writer-{id}"))
        .spawn(move || writer_loop(writer_shared, id, writer_stream, receiver));
    if let Err(error) = writer {
        drop(core);
        log::warn!("could not spawn writer for {name}: {error}");
        reject_handshake(&stream, "server_error");
        return;
    }

    let default_mode = core.config.game_mode;
    let Some(net_player) = core.host.join_network_player(id, &name, default_mode).cloned() else {
        drop(core);
        reject_handshake(&stream, "server_error");
        return;
    };
    let self_snapshot = PlayerSnapshot {
        id,
        name: name.clone(),
        pos: [net_player.pos.x, net_player.pos.y, net_player.pos.z],
        yaw: net_player.yaw,
        pitch: net_player.pitch,
    };
    let mut roster: Vec<PlayerSnapshot> = core.players.values().map(NetPlayer::snapshot).collect();
    roster.push(self_snapshot);
    let hello = ServerMessage::Hello {
        protocol: PROTOCOL_VERSION,
        world_name: core.world_name.clone(),
        seed: core.seed,
        spawn: core.spawn,
        content_fingerprint: core.fingerprint,
        tick: core.host.tick_count(),
        day_ticks: ob_protocol::DAY_LENGTH_TICKS,
        max_players: core.config.max_players,
        motd: core.config.motd.clone(),
        blocks: core.blocks.clone(),
    };
    let mut initial = Vec::with_capacity(8 + roster.len());
    initial.push(hello);
    initial.push(ServerMessage::Join { id, name: name.clone() });
    for snapshot in &roster {
        initial.push(ServerMessage::PlayerState {
            id: snapshot.id,
            name: snapshot.name.clone(),
            pos: snapshot.pos,
            yaw: snapshot.yaw,
            pitch: snapshot.pitch,
        });
    }
    initial.push(ServerMessage::PlayerList {
        players: roster.clone(),
    });
    initial.push(inventory_sync_message(&core.host, &net_player));
    initial.push(vitals_message(&core.host, &net_player));
    initial.push(ServerMessage::SelectedSlot {
        slot: net_player.selected_slot as u8,
    });
    if net_player.dead {
        initial.push(ServerMessage::Death {
            cause: net_player
                .last_damage_cause
                .clone()
                .unwrap_or_else(|| "rejoined while dead".to_string()),
        });
    }
    let (entities, known_entities) = initial_entities(&core.host, net_player.pos);
    initial.push(ServerMessage::EntitySnapshot { entities });

    let mut queued = true;
    for message in initial {
        match encode_server_frame(&message) {
            Ok(frame) => {
                if sender.try_send(frame).is_err() {
                    queued = false;
                    break;
                }
            }
            Err(error) => {
                log::error!("could not encode handshake message: {error}");
                queued = false;
                break;
            }
        }
    }
    if !queued {
        core.host.leave_network_player(id);
        drop(core);
        reject_handshake(&stream, "outbound queue full");
        return;
    }

    // Tell existing players before the new player becomes visible to the tick
    // loop, so nobody receives a PlayerState for an unknown id.
    if let Ok(join_frame) = encode_server_frame(&ServerMessage::Join { id, name: name.clone() }) {
        let failed = core.queue_to_all(&join_frame, None);
        for failed_id in failed {
            core.disconnect(failed_id, None, "outbound queue full");
        }
    }

    let player = NetPlayer {
        id,
        name: name.clone(),
        pos: net_player.pos,
        yaw: net_player.yaw,
        pitch: net_player.pitch,
        sender,
        stream: match stream.try_clone() {
            Ok(stream) => stream,
            Err(error) => {
                core.host.leave_network_player(id);
                drop(core);
                log::warn!("handshake failed: {error}");
                return;
            }
        },
        sent_chunks: HashSet::new(),
        last_move: Instant::now(),
        move_limit: RateLimit::new(MOVES_PER_SECOND, 30),
        edit_limit: RateLimit::new(EDITS_PER_SECOND, 20),
        use_limit: RateLimit::new(USE_BLOCKS_PER_SECOND, 5),
        attack_limit: RateLimit::new(ATTACKS_PER_SECOND, 10),
        inventory_limit: RateLimit::new(INVENTORY_MOVES_PER_SECOND, 30),
        eat_limit: RateLimit::new(EAT_PER_SECOND, 4),
        equip_limit: RateLimit::new(EQUIPS_PER_SECOND, 5),
        select_limit: RateLimit::new(SELECTS_PER_SECOND, 20),
        respawn_limit: RateLimit::new(RESPAWNS_PER_SECOND, 2),
        close_limit: RateLimit::new(CONTAINER_CLOSES_PER_SECOND, 10),
        chat_limit: RateLimit::new(CHAT_PER_SECOND, 5),
        chunk_limit: RateLimit::new(CHUNK_REQUESTS_PER_SECOND, 8),
        ping_limit: RateLimit::new(10.0, 10),
        window: None,
        known_entities,
    };
    core.players.insert(id, player);
    drop(core);

    let reader_shared = Arc::clone(&shared);
    let reader = thread::Builder::new()
        .name(format!("ob-net-reader-{id}"))
        .spawn(move || reader_loop(reader_shared, id, stream));
    if let Err(error) = reader {
        log::warn!("could not spawn reader for {name}: {error}");
        lock(&shared.core).disconnect(id, Some("server_error".to_string()), "reader spawn failed");
        return;
    }
    log::info!("player '{name}' joined as id {id}");
}

/// Sends a best-effort Disconnect frame and closes a rejected connection.
fn reject_handshake(stream: &TcpStream, reason: &str) {
    let message = ServerMessage::Disconnect {
        reason: sanitise_reason(reason),
    };
    if let Ok(frame) = encode_server_frame(&message) {
        let mut stream = stream;
        let _ = stream.write_all(&frame);
        let _ = stream.flush();
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Writes queued frames in order. A write error is reported to the tick thread,
/// which removes the player.
fn writer_loop(shared: Arc<Shared>, id: u64, mut stream: TcpStream, receiver: Receiver<Vec<u8>>) {
    while let Ok(frame) = receiver.recv() {
        if let Err(error) = stream.write_all(&frame) {
            log::debug!("writer for client {id} failed: {error}");
            let _ = shared.events.send(ServerEvent::WriterFailed { id });
            break;
        }
        shared
            .metrics
            .bytes_out
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
    }
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

/// Reads frames and forwards decoded intents to the tick thread.
fn reader_loop(shared: Arc<Shared>, id: u64, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut limit = RateLimit::new(INBOUND_FRAMES_PER_SECOND, INBOUND_BURST);
    loop {
        let payload = match read_frame(&mut stream, MAX_FRAME_LEN) {
            Ok(payload) => payload,
            Err(error) => {
                let reason = frame_error_reason(&error);
                let _ = shared.events.send(ServerEvent::Closed { id, reason });
                return;
            }
        };
        shared
            .metrics
            .bytes_in
            .fetch_add(payload.len() as u64 + 4, Ordering::Relaxed);
        if !limit.allow_now() {
            let _ = shared.events.send(ServerEvent::Closed {
                id,
                reason: Some("rate_limited".to_string()),
            });
            return;
        }
        match decode_payload::<ClientMessage>(&payload, MAX_FRAME_LEN) {
            Ok(ClientMessage::Disconnect) => {
                let _ = shared.events.send(ServerEvent::Closed { id, reason: None });
                return;
            }
            Ok(message) => {
                if shared.events.send(ServerEvent::Message { id, message }).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = shared.events.send(ServerEvent::Closed {
                    id,
                    reason: Some(format!("protocol_error: {error}")),
                });
                return;
            }
        }
    }
}

/// Maps a handshake framing/decoding error to a short reason code.
fn handshake_error_reason(error: &ob_protocol::ProtocolError) -> String {
    frame_error_reason(error).unwrap_or_else(|| format!("protocol_error: {error}"))
}

/// Maps a framing error to a short protocol reason code.
fn frame_error_reason(error: &ob_protocol::ProtocolError) -> Option<String> {
    match error {
        ob_protocol::ProtocolError::FrameTooLarge { .. } => Some("frame_too_large".to_string()),
        ob_protocol::ProtocolError::EmptyFrame => Some("protocol_error: empty frame".to_string()),
        ob_protocol::ProtocolError::TruncatedFrame => Some("protocol_error: truncated frame".to_string()),
        ob_protocol::ProtocolError::Json(_) => Some("protocol_error: invalid json".to_string()),
        ob_protocol::ProtocolError::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ) =>
        {
            None
        }
        ob_protocol::ProtocolError::Io(error)
            if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) =>
        {
            Some("timeout".to_string())
        }
        ob_protocol::ProtocolError::Io(_) => None,
        other => Some(format!("protocol_error: {other}")),
    }
}

/// Tick loop: drains intents, advances the host, streams chunks, replicates
/// entities, updates windows and autosaves at 20 Hz.
fn run_tick_loop(shared: Arc<Shared>, receiver: Receiver<ServerEvent>) {
    let mut next_tick = Instant::now();
    let mut window_start = Instant::now();
    let mut ticks_in_window = 0u32;
    let mut last_log = Instant::now();

    while !shared.shutdown.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now < next_tick {
            thread::sleep((next_tick - now).min(Duration::from_millis(5)));
            continue;
        }
        next_tick += TICK_INTERVAL;
        if next_tick < now {
            next_tick = now + TICK_INTERVAL;
        }
        let tick_start = Instant::now();
        let tick_duration;
        {
            let mut core = lock(&shared.core);
            core.process_events(&receiver);
            core.tick(now);
            tick_duration = tick_start.elapsed();
            if tick_duration > Duration::from_millis(45) {
                log::warn!("tick overran the 50 ms budget: {tick_duration:?}");
            }
        }
        shared.metrics.record_tick(tick_duration);
        ticks_in_window += 1;
        if now.duration_since(window_start) >= Duration::from_secs(1) {
            let tps = ticks_in_window as f32 / now.duration_since(window_start).as_secs_f32();
            let mut core = lock(&shared.core);
            core.tps = tps;
            if now.duration_since(last_log) >= Duration::from_secs(5) {
                log::info!(
                    "tps {:.1}, players {}, chunks {}, edits {} ({} rejected), last tick {:?}",
                    tps,
                    core.players.len(),
                    core.host.world().chunk_count(),
                    core.edits_accepted,
                    core.edits_rejected,
                    tick_duration
                );
                last_log = now;
            }
            ticks_in_window = 0;
            window_start = now;
        }
    }

    let mut core = lock(&shared.core);
    core.close_all("server_shutdown");
    log::info!(
        "network server stopped after {} ticks ({} edits accepted, {} rejected)",
        core.tick_count,
        core.edits_accepted,
        core.edits_rejected
    );
}

/// Builds a complete `InventorySync` for one network player.
fn inventory_sync_message(host: &Host, player: &ServerPlayer) -> ServerMessage {
    let registry = host.registry();
    let hotbar: [Option<ItemStackDto>; HOTBAR_SLOTS] =
        std::array::from_fn(|slot| stack_dto(registry, &player.inventory[slot]));
    let equipment: [Option<ItemStackDto>; EQUIPMENT_SLOTS] =
        std::array::from_fn(|slot| stack_dto(registry, &player.equipment[slot]));
    ServerMessage::InventorySync {
        hotbar,
        selected: player.selected_slot.min(HOTBAR_SLOTS - 1) as u8,
        equipment,
    }
}

/// Builds a `PlayerVitals` message for one network player.
fn vitals_message(host: &Host, player: &ServerPlayer) -> ServerMessage {
    ServerMessage::PlayerVitals {
        health: player.health,
        hunger: player.hunger,
        armor_defense: player.armor_defense(host.registry()),
    }
}

/// Builds the complete entity snapshot visible to a newly joined player.
fn initial_entities(host: &Host, pos: WorldVec) -> (Vec<EntitySnapshot>, BTreeMap<u64, EntityView>) {
    let views = world_entity_views(host);
    let mut entities = Vec::new();
    let mut known = BTreeMap::new();
    for (id, view) in views {
        if WorldVec::new(view.pos[0], view.pos[1], view.pos[2]).distance(pos) > ENTITY_INTEREST_RADIUS {
            continue;
        }
        entities.push(view.snapshot());
        known.insert(id, view);
    }
    (entities, known)
}

/// Builds the authoritative entity view map for mobs and dropped items.
fn world_entity_views(host: &Host) -> BTreeMap<u64, EntityView> {
    let registry = host.registry();
    let mut views = BTreeMap::new();
    for mob in host.mobs() {
        let view = EntityView {
            id: mob.id,
            kind: EntityKind::Mob {
                mob: mob.mob.kind.name().to_string(),
            },
            pos: [mob.mob.pos.x, mob.mob.pos.y, mob.mob.pos.z],
            yaw: mob.mob.yaw,
            health: mob.mob.health,
            hurt: mob.mob.hurt_timer > 0.0,
            stack: None,
        };
        views.insert(mob.id, view);
    }
    for item in host.item_entities() {
        let Some(stack) = stack_dto(registry, &Some(item.stack)) else {
            continue;
        };
        let view = EntityView {
            id: wire_entity_id(true, item.id),
            kind: EntityKind::Item { stack: stack.clone() },
            pos: [item.pos.x, item.pos.y, item.pos.z],
            yaw: 0.0,
            health: 0.0,
            hurt: false,
            stack: Some(stack),
        };
        views.insert(wire_entity_id(true, item.id), view);
    }
    views
}

impl Core {
    /// Drains all events currently queued by reader/writer threads.
    fn process_events(&mut self, receiver: &Receiver<ServerEvent>) {
        while let Ok(event) = receiver.try_recv() {
            self.handle_event(event);
        }
    }

    /// Handles one reader/writer event.
    fn handle_event(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::Message { id, message } => {
                if self.players.contains_key(&id) {
                    self.handle_message(id, message);
                }
            }
            ServerEvent::Closed { id, reason } => {
                let log_reason = if reason.is_some() {
                    "connection error"
                } else {
                    "connection closed"
                };
                self.disconnect(id, reason, log_reason);
            }
            ServerEvent::WriterFailed { id } => {
                self.disconnect(id, None, "write failed");
            }
        }
    }

    /// Dispatches one playing-state message.
    fn handle_message(&mut self, id: u64, message: ClientMessage) {
        let now = Instant::now();
        match message {
            ClientMessage::Hello { .. } => {
                self.disconnect(
                    id,
                    Some("protocol_error: duplicate hello".to_string()),
                    "duplicate hello",
                );
            }
            ClientMessage::ChunkRequest { cx, cz } => {
                Metrics::count(&self.metrics.chunk_requests);
                self.handle_chunk_request(id, cx, cz, now);
            }
            ClientMessage::Move { pos, yaw, pitch } => {
                Metrics::count(&self.metrics.moves);
                self.handle_move(id, pos, yaw, pitch, now);
            }
            ClientMessage::BlockEdit { pos, block } => {
                Metrics::count(&self.metrics.edits_attempted);
                self.handle_block_edit(id, pos, block, now);
            }
            ClientMessage::SelectSlot { slot } => self.handle_select_slot(id, slot, now),
            ClientMessage::UseBlock { pos } => self.handle_use_block(id, pos, now),
            ClientMessage::Attack { target } => self.handle_attack(id, target, now),
            ClientMessage::InventoryMove {
                from,
                to,
                count,
                revision,
            } => self.handle_inventory_move(id, from, to, count, revision, now),
            ClientMessage::Eat => self.handle_eat(id, now),
            ClientMessage::Equip { hotbar_slot } => self.handle_equip(id, hotbar_slot, now),
            ClientMessage::Respawn => self.handle_respawn(id, now),
            ClientMessage::ContainerClose => self.handle_container_close(id, now),
            ClientMessage::Chat { text } => {
                Metrics::count(&self.metrics.chats);
                self.handle_chat(id, text, now);
            }
            ClientMessage::Ping { id: nonce } => {
                Metrics::count(&self.metrics.pings);
                let allowed = self
                    .players
                    .get_mut(&id)
                    .is_some_and(|player| player.ping_limit.allow(now));
                if allowed {
                    self.send(id, ServerMessage::Pong { id: nonce });
                }
            }
            ClientMessage::Disconnect => {
                self.disconnect(id, None, "client disconnect");
            }
        }
    }

    /// Applies a client-authoritative move after validation and clamping.
    fn handle_move(&mut self, id: u64, pos: [f64; 3], yaw: f32, pitch: f32, now: Instant) {
        let Some(player) = self.players.get_mut(&id) else {
            return;
        };
        if !player.move_limit.allow(now) {
            return;
        }
        if !pos.iter().all(|value| value.is_finite()) || !yaw.is_finite() || !pitch.is_finite() {
            return;
        }
        let next = WorldVec::new(
            pos[0].clamp(-MAX_WORLD_XZ, MAX_WORLD_XZ),
            pos[1].clamp(MIN_PLAYER_Y, MAX_PLAYER_Y),
            pos[2].clamp(-MAX_WORLD_XZ, MAX_WORLD_XZ),
        );
        let distance = next.distance(player.pos);
        if distance > MAX_MOVE_DISTANCE {
            return;
        }
        // Client-authoritative movement with a generous per-message allowance:
        // a strict horizontal gate plus a larger vertical gate so a real
        // terminal-velocity fall is accepted. Larger jumps are dropped.
        let elapsed = now.saturating_duration_since(player.last_move).as_secs_f64();
        let dx = next.x - player.pos.x;
        let dz = next.z - player.pos.z;
        let horizontal = (dx * dx + dz * dz).sqrt();
        let vertical = (next.y - player.pos.y).abs();
        let horizontal_allowance = (elapsed * MOVE_ALLOWANCE_PER_SECOND).max(MOVE_MIN_ALLOWANCE);
        let vertical_allowance = (elapsed * FALL_ALLOWANCE_PER_SECOND).max(FALL_MIN_ALLOWANCE);
        if horizontal > horizontal_allowance || vertical > vertical_allowance {
            log::debug!(
                "rejecting move for player {id}: horizontal {horizontal:.2}/{horizontal_allowance:.2} m, vertical {vertical:.2}/{vertical_allowance:.2} m"
            );
            return;
        }
        player.last_move = now;
        let Some(result) = self.host.update_network_player_move(id, next, yaw, pitch) else {
            return;
        };
        if let Some(player) = self.players.get_mut(&id) {
            player.pos = result.pos;
            player.yaw = yaw % std::f32::consts::TAU;
            if player.yaw > std::f32::consts::PI {
                player.yaw -= std::f32::consts::TAU;
            } else if player.yaw < -std::f32::consts::PI {
                player.yaw += std::f32::consts::TAU;
            }
            player.pitch = pitch.clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
        }
        if result.fall_damage > 0.0 || result.died {
            self.relay_damage_events();
        }
    }

    /// Applies one creative remote edit or survival break/place intent.
    fn handle_block_edit(&mut self, id: u64, pos: [i32; 3], block: u16, now: Instant) {
        let Some(player) = self.players.get_mut(&id) else {
            return;
        };
        if !player.edit_limit.allow(now) {
            self.edits_rejected += 1;
            return;
        }
        let origin = player.pos;
        let block_pos = BlockPos::new(pos[0], pos[1], pos[2]);
        if !block_pos.in_world() || block_pos.center().distance(origin) > MAX_EDIT_DISTANCE {
            self.edits_rejected += 1;
            self.send_action_rejected(id, "block_edit", "out_of_reach");
            return;
        }
        let Some(actor) = self.host.network_player(id) else {
            return;
        };
        if actor.dead {
            self.edits_rejected += 1;
            self.send_action_rejected(id, "block_edit", "dead");
            return;
        }
        let creative = actor.game_mode == GameMode::Creative;

        if creative {
            if block as usize >= self.host.registry().len() {
                self.edits_rejected += 1;
                return;
            }
            let block = BlockId(block);
            if !self.host.apply_remote_edit(block_pos, block) {
                self.edits_rejected += 1;
                self.send_action_rejected(id, "block_edit", "rejected");
                return;
            }
            self.edits_accepted += 1;
            self.dirty = true;
            self.close_windows_at(block_pos, "block_removed");
            self.broadcast(
                &ServerMessage::BlockEdit {
                    pos,
                    block: block.0,
                    player: id,
                },
                None,
            );
            return;
        }

        let outcome = if block == 0 {
            self.host.network_break_block(id, block_pos)
        } else {
            if block as usize >= self.host.registry().len() {
                self.edits_rejected += 1;
                self.send_action_rejected(id, "block_edit", "invalid_block");
                return;
            }
            self.host.network_place_block(id, block_pos, BlockId(block))
        };
        match outcome {
            Ok(outcome) if outcome.changed => {
                self.edits_accepted += 1;
                self.dirty = true;
                let new_block = outcome.placed.map_or(0, |block| block.0);
                if outcome.broke.is_some() {
                    self.close_windows_at(block_pos, "block_removed");
                }
                self.broadcast(
                    &ServerMessage::BlockEdit {
                        pos,
                        block: new_block,
                        player: id,
                    },
                    None,
                );
                self.send_action_sync(id);
            }
            Ok(_) => {
                self.edits_rejected += 1;
                self.send_action_rejected(id, "block_edit", "rejected");
            }
            Err(reason) => {
                self.edits_rejected += 1;
                self.send_action_rejected(id, "block_edit", &reason);
            }
        }
    }

    /// Selects a hotbar slot.
    fn handle_select_slot(&mut self, id: u64, slot: u8, now: Instant) {
        let allowed = self
            .players
            .get_mut(&id)
            .is_some_and(|player| player.select_limit.allow(now));
        if !allowed {
            return;
        }
        if usize::from(slot) >= HOTBAR_SLOTS {
            self.send_action_rejected(id, "select_slot", "invalid_slot");
            return;
        }
        if self.host.select_network_slot(id, usize::from(slot)) {
            self.dirty = true;
            self.send(id, ServerMessage::SelectedSlot { slot });
        } else {
            self.send_action_rejected(id, "select_slot", "dead");
        }
    }

    /// Opens a furnace/chest window after validating block, distance and
    /// ownership.
    fn handle_use_block(&mut self, id: u64, pos: [i32; 3], now: Instant) {
        let allowed = self
            .players
            .get_mut(&id)
            .is_some_and(|player| player.use_limit.allow(now));
        if !allowed {
            return;
        }
        let block_pos = BlockPos::new(pos[0], pos[1], pos[2]);
        let Some(player) = self.host.network_player(id) else {
            return;
        };
        if player.dead {
            self.send_action_rejected(id, "use_block", "dead");
            return;
        }
        if !block_pos.in_world() || block_pos.center().distance(player.pos) > MAX_CONTAINER_DISTANCE {
            self.send_action_rejected(id, "use_block", "out_of_reach");
            return;
        }
        if self.host.world().chunk(block_pos.chunk()).is_none() {
            self.send_action_rejected(id, "use_block", "unloaded");
            return;
        }
        let block = self.host.world().get_block(block_pos);
        let furnace_id = self.host.registry().id_or_air("openblock:furnace");
        let chest_id = self.host.registry().id_or_air("openblock:chest");
        let kind = if block == furnace_id && self.host.furnace_at(block_pos).is_some() {
            ContainerKind::Furnace
        } else if block == chest_id && self.host.chest_at(block_pos).is_some() {
            ContainerKind::Chest
        } else {
            self.send_action_rejected(id, "use_block", "not_container");
            return;
        };

        // Only one window per player; opening a new one closes the old one.
        if self
            .players
            .get(&id)
            .and_then(|player| player.window.as_ref())
            .is_some()
        {
            self.close_window(id, "replaced", true);
        }
        let Some(slot_stacks) = self.container_slots(block_pos, kind) else {
            self.send_action_rejected(id, "use_block", "container_missing");
            return;
        };
        let furnace = if kind == ContainerKind::Furnace {
            self.furnace_progress(block_pos)
        } else {
            None
        };
        let window_id = self.next_window_id.max(1);
        self.next_window_id = self.next_window_id.wrapping_add(1).max(1);
        let window = OpenWindow {
            id: window_id,
            pos: block_pos,
            kind,
            revision: 1,
            slots: slot_stacks,
            furnace,
        };
        let message = ServerMessage::ContainerOpen {
            window: window_id,
            kind,
            pos,
            revision: window.revision,
            slots: window_slot_dtos(&self.host, &window.slots),
            furnace,
        };
        if let Some(net_player) = self.players.get_mut(&id) {
            net_player.window = Some(window);
        }
        self.send(id, message);
    }

    /// Closes the sending player's window on request.
    fn handle_container_close(&mut self, id: u64, now: Instant) {
        let allowed = self
            .players
            .get_mut(&id)
            .is_some_and(|player| player.close_limit.allow(now));
        if allowed {
            self.close_window(id, "closed", true);
        }
    }

    /// Applies one inventory/equipment/window transaction.
    fn handle_inventory_move(&mut self, id: u64, from: u16, to: u16, count: u8, revision: Option<u64>, now: Instant) {
        let allowed = self
            .players
            .get_mut(&id)
            .is_some_and(|player| player.inventory_limit.allow(now));
        if !allowed {
            return;
        }
        let Some(actor) = self.host.network_player(id) else {
            return;
        };
        if actor.dead {
            self.send_action_rejected(id, "inventory_move", "dead");
            return;
        }
        let window = self.players.get(&id).and_then(|player| player.window.as_ref());
        if (from >= SLOT_WINDOW_START || to >= SLOT_WINDOW_START) && window.is_none() {
            self.send_action_rejected(id, "inventory_move", "no_window");
            return;
        }
        if let (Some(window), Some(revision)) = (window, revision)
            && revision != window.revision
        {
            self.send_action_rejected(id, "inventory_move", "stale_revision");
            return;
        }
        let window_ref = window.map(OpenWindow::window_ref);
        let involved_window = window_ref.is_some_and(|_| from >= SLOT_WINDOW_START || to >= SLOT_WINDOW_START);
        match self.host.network_inventory_move(id, from, to, count, window_ref) {
            Ok(changed) if changed => {
                self.dirty = true;
                if involved_window {
                    self.refresh_window(id);
                }
                self.send_action_sync(id);
            }
            Ok(_) => {}
            Err(reason) => self.send_action_rejected(id, "inventory_move", reason),
        }
    }

    /// Eats from the selected hotbar slot.
    fn handle_eat(&mut self, id: u64, now: Instant) {
        let allowed = self
            .players
            .get_mut(&id)
            .is_some_and(|player| player.eat_limit.allow(now));
        if !allowed {
            return;
        }
        if self.host.network_eat(id) {
            self.dirty = true;
            self.send_action_sync(id);
        } else {
            self.send_action_rejected(id, "eat", "not_edible_or_full");
        }
    }

    /// Equips the armor piece in one hotbar slot.
    fn handle_equip(&mut self, id: u64, hotbar_slot: u8, now: Instant) {
        let allowed = self
            .players
            .get_mut(&id)
            .is_some_and(|player| player.equip_limit.allow(now));
        if !allowed {
            return;
        }
        if self.host.network_player(id).is_some_and(|player| player.dead) {
            self.send_action_rejected(id, "equip", "dead");
            return;
        }
        if self.host.network_equip(id, usize::from(hotbar_slot)) {
            self.dirty = true;
            self.send_action_sync(id);
        } else {
            self.send_action_rejected(id, "equip", "not_equippable");
        }
    }

    /// Respawns a dead player at the server spawn, keeping inventory.
    fn handle_respawn(&mut self, id: u64, now: Instant) {
        let allowed = self
            .players
            .get_mut(&id)
            .is_some_and(|player| player.respawn_limit.allow(now));
        if !allowed {
            return;
        }
        if !self.host.respawn_network_player(id) {
            self.send_action_rejected(id, "respawn", "not_dead");
            return;
        }
        self.dirty = true;
        let Some(player) = self.host.network_player(id) else {
            return;
        };
        let pos = player.pos;
        let health = player.health;
        let hunger = player.hunger;
        if let Some(net_player) = self.players.get_mut(&id) {
            net_player.pos = pos;
            net_player.last_move = now;
        }
        self.send(
            id,
            ServerMessage::Respawn {
                pos: [pos.x, pos.y, pos.z],
                health,
                hunger,
            },
        );
        self.send_action_sync(id);
        self.broadcast_states();
    }

    /// Applies one melee attack intent.
    fn handle_attack(&mut self, id: u64, target: Option<u64>, now: Instant) {
        let allowed = self
            .players
            .get_mut(&id)
            .is_some_and(|player| player.attack_limit.allow(now));
        if !allowed {
            return;
        }
        match self.host.network_attack(id, target) {
            Ok(outcome) => {
                self.dirty = true;
                self.send(
                    id,
                    ServerMessage::AttackResult {
                        target: outcome.id,
                        hit: outcome.hit,
                        damage: outcome.damage,
                        killed: outcome.killed,
                    },
                );
                if outcome.tool_broke.is_some() {
                    self.send_action_sync(id);
                }
                if outcome.killed {
                    self.broadcast_entity_despawn(&[outcome.id], DespawnReason::Killed);
                }
            }
            Err(reason) => self.send_action_rejected(id, "attack", &reason),
        }
    }

    /// Relays a validated chat line to everyone, including its sender.
    fn handle_chat(&mut self, id: u64, text: String, now: Instant) {
        let Ok(text) = validate_chat_text(&text) else {
            return;
        };
        let Some(player) = self.players.get_mut(&id) else {
            return;
        };
        if !player.chat_limit.allow(now) {
            return;
        }
        let from = player.name.clone();
        // Chat content is not logged by default (20 §3.14): keep only the
        // sender and length at debug level so an operator can enable it
        // deliberately, never as an incidental info-level side effect.
        log::debug!("chat from '{from}' ({} chars)", text.chars().count());
        self.broadcast(&ServerMessage::Chat { from, text }, None);
    }

    /// Serves one explicit chunk request within the 16-chunk admission radius.
    fn handle_chunk_request(&mut self, id: u64, cx: i32, cz: i32, now: Instant) {
        let Some(player) = self.players.get_mut(&id) else {
            return;
        };
        if !player.chunk_limit.allow(now) {
            return;
        }
        let center = BlockPos::new(
            player.pos.x.floor() as i32,
            player.pos.y.floor() as i32,
            player.pos.z.floor() as i32,
        )
        .chunk();
        let dx = i64::from(cx) - i64::from(center.x);
        let dz = i64::from(cz) - i64::from(center.z);
        if dx.abs() > i64::from(MAX_INTEREST_RADIUS_CHUNKS) || dz.abs() > i64::from(MAX_INTEREST_RADIUS_CHUNKS) {
            return;
        }
        let requested = ChunkPos::new(cx, cz);
        let world_x = f64::from(cx) * f64::from(ob_math::CHUNK_SIZE) + 8.0;
        let world_z = f64::from(cz) * f64::from(ob_math::CHUNK_SIZE) + 8.0;
        self.host
            .ensure_streaming_around(WorldVec::new(world_x, player.pos.y, world_z), 0, 1);
        let Some(chunk) = self.host.chunk_column(requested) else {
            return;
        };
        match encode_chunk_checked(self.host.registry(), chunk) {
            Ok(bytes) => {
                self.send(id, ServerMessage::ChunkData { cx, cz, bytes });
                if let Some(player) = self.players.get_mut(&id) {
                    player.sent_chunks.insert(requested);
                }
            }
            Err(error) => log::warn!("could not encode requested chunk {cx},{cz}: {error}"),
        }
    }

    /// Sends a full inventory/vitals/selection refresh after a successful
    /// action.
    fn send_action_sync(&mut self, id: u64) {
        let messages = {
            let Some(player) = self.host.network_player(id) else {
                return;
            };
            vec![
                inventory_sync_message(&self.host, player),
                vitals_message(&self.host, player),
                ServerMessage::SelectedSlot {
                    slot: player.selected_slot as u8,
                },
            ]
        };
        self.host.take_network_inventory_dirty(id);
        self.host.take_network_vitals_dirty(id);
        for message in messages {
            self.send(id, message);
        }
    }

    /// Sends a short rejection hint to one player.
    fn send_action_rejected(&mut self, id: u64, action: &str, reason: &str) {
        Metrics::count(&self.metrics.actions_rejected);
        self.send(
            id,
            ServerMessage::ActionRejected {
                action: action.to_string(),
                reason: sanitise_reason(reason),
            },
        );
    }

    /// Sends fresh vitals to one player and clears the dirty flag.
    fn send_vitals_now(&mut self, id: u64) {
        let message = self
            .host
            .network_player(id)
            .map(|player| vitals_message(&self.host, player));
        self.host.take_network_vitals_dirty(id);
        if let Some(message) = message {
            self.send(id, message);
        }
    }

    /// Relays damage/death events produced by movement, mobs, starvation or
    /// admin damage.
    fn relay_damage_events(&mut self) {
        for event in self.host.take_network_damage_events() {
            if !self.players.contains_key(&event.player) {
                continue;
            }
            if event.amount > 0.0 {
                self.send(
                    event.player,
                    ServerMessage::DamageEvent {
                        amount: event.amount,
                        cause: sanitise_reason(&event.cause),
                    },
                );
            }
            self.send_vitals_now(event.player);
            if event.died {
                self.close_window(event.player, "death", true);
                self.send(
                    event.player,
                    ServerMessage::Death {
                        cause: sanitise_reason(&event.cause),
                    },
                );
            }
        }
    }

    /// Relays host-side entity removals (picked-up items, burned/despawned
    /// mobs) before the periodic diff.
    fn relay_entity_removals(&mut self) {
        for removed in self.host.take_network_entity_removals() {
            let id = wire_entity_id(removed.item, removed.id);
            self.broadcast_entity_despawn(&[id], removed.reason);
        }
    }

    /// Reads the authoritative slot contents of a container.
    fn container_slots(&self, pos: BlockPos, kind: ContainerKind) -> Option<Vec<Option<ItemStack>>> {
        match kind {
            ContainerKind::Furnace => {
                let state = self.host.furnace_at(pos)?;
                Some(vec![state.input, state.fuel, state.output])
            }
            ContainerKind::Chest => {
                let state = self.host.chest_at(pos)?;
                Some(state.slots.to_vec())
            }
        }
    }

    /// Reads the furnace progress fields, if the position is a furnace.
    fn furnace_progress(&self, pos: BlockPos) -> Option<FurnaceProgress> {
        let state = self.host.furnace_at(pos)?;
        Some(FurnaceProgress {
            burn_remaining: state.burn_remaining,
            burn_total: state.burn_total,
            cook_progress: state.cook_progress,
        })
    }

    /// Re-reads an open window after a transaction, bumps its revision when
    /// contents changed and sends `ContainerUpdate`.
    fn refresh_window(&mut self, id: u64) {
        let Some(mut window) = self.players.get_mut(&id).and_then(|player| player.window.take()) else {
            return;
        };
        let Some(slots) = self.container_slots(window.pos, window.kind) else {
            self.send(
                id,
                ServerMessage::ContainerClose {
                    window: window.id,
                    reason: "block_removed".to_string(),
                },
            );
            return;
        };
        let furnace = if window.kind == ContainerKind::Furnace {
            self.furnace_progress(window.pos)
        } else {
            None
        };
        let changed = slots != window.slots;
        if changed {
            window.revision = window.revision.saturating_add(1);
        }
        window.slots = slots;
        window.furnace = furnace;
        let message = ServerMessage::ContainerUpdate {
            window: window.id,
            revision: window.revision,
            slots: window_slot_dtos(&self.host, &window.slots),
            furnace,
        };
        if let Some(player) = self.players.get_mut(&id) {
            player.window = Some(window);
        }
        self.send(id, message);
    }

    /// Closes one player's window and optionally notifies them.
    fn close_window(&mut self, id: u64, reason: &str, notify: bool) {
        let Some(window) = self.players.get_mut(&id).and_then(|player| player.window.take()) else {
            return;
        };
        if notify {
            self.send(
                id,
                ServerMessage::ContainerClose {
                    window: window.id,
                    reason: sanitise_reason(reason),
                },
            );
        }
    }

    /// Closes every window pointing at `pos` (block broken or replaced).
    fn close_windows_at(&mut self, pos: BlockPos, reason: &str) {
        let ids: Vec<u64> = self
            .players
            .iter()
            .filter(|(_, player)| player.window.as_ref().is_some_and(|window| window.pos == pos))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.close_window(id, reason, true);
        }
    }

    /// Validates distances/death and sends throttled furnace progress updates.
    fn tick_windows(&mut self) {
        let ids: Vec<u64> = self.players.keys().copied().collect();
        for id in ids {
            let Some(mut window) = self.players.get_mut(&id).and_then(|player| player.window.take()) else {
                continue;
            };
            let Some(player_pos) = self.players.get(&id).map(|player| player.pos) else {
                continue;
            };
            let dead = self.host.network_player(id).is_some_and(|player| player.dead);
            if dead {
                self.send(
                    id,
                    ServerMessage::ContainerClose {
                        window: window.id,
                        reason: "death".to_string(),
                    },
                );
                continue;
            }
            let furnace_id = self.host.registry().id_or_air("openblock:furnace");
            let chest_id = self.host.registry().id_or_air("openblock:chest");
            let (expected, exists) = match window.kind {
                ContainerKind::Furnace => (furnace_id, self.host.furnace_at(window.pos).is_some()),
                ContainerKind::Chest => (chest_id, self.host.chest_at(window.pos).is_some()),
            };
            if !exists
                || self.host.world().get_block(window.pos) != expected
                || window.pos.center().distance(player_pos) > MAX_CONTAINER_DISTANCE
            {
                self.send(
                    id,
                    ServerMessage::ContainerClose {
                        window: window.id,
                        reason: "out_of_range".to_string(),
                    },
                );
                continue;
            }
            if !self.tick_count.is_multiple_of(WINDOW_UPDATE_INTERVAL_TICKS) {
                if let Some(player) = self.players.get_mut(&id) {
                    player.window = Some(window);
                }
                continue;
            }
            let Some(slots) = self.container_slots(window.pos, window.kind) else {
                self.send(
                    id,
                    ServerMessage::ContainerClose {
                        window: window.id,
                        reason: "block_removed".to_string(),
                    },
                );
                continue;
            };
            let furnace = if window.kind == ContainerKind::Furnace {
                self.furnace_progress(window.pos)
            } else {
                None
            };
            let slots_changed = slots != window.slots;
            let progress_changed = furnace != window.furnace;
            if slots_changed {
                window.revision = window.revision.saturating_add(1);
            }
            window.slots = slots;
            window.furnace = furnace;
            let message =
                (slots_changed || (furnace.is_some() && progress_changed)).then(|| ServerMessage::ContainerUpdate {
                    window: window.id,
                    revision: window.revision,
                    slots: window_slot_dtos(&self.host, &window.slots),
                    furnace,
                });
            if let Some(player) = self.players.get_mut(&id) {
                player.window = Some(window);
            }
            if let Some(message) = message {
                self.send(id, message);
            }
        }
    }

    /// Advances the host, then relays results, streams chunks, replicates
    /// entities, updates windows, syncs players and autosaves.
    fn tick(&mut self, now: Instant) {
        // The host's internal single player is intentionally unused by network
        // play; `tick_network` advances only network players, mobs, furnaces
        // and dropped items.
        self.host.tick_network();
        self.tick_count += 1;
        self.relay_damage_events();
        self.relay_entity_removals();
        self.stream_chunks();
        self.tick_windows();
        if self.tick_count.is_multiple_of(ENTITY_SYNC_INTERVAL_TICKS) {
            self.replicate_entities();
            self.broadcast_states();
        }
        self.sync_players();
        self.maybe_autosave(now);
    }

    /// Sends spawns/changes/removals for mobs and dropped items inside each
    /// player's interest radius.
    fn replicate_entities(&mut self) {
        if self.players.is_empty() {
            return;
        }
        let views = world_entity_views(&self.host);
        let ids: Vec<u64> = self.players.keys().copied().collect();
        for id in ids {
            let Some(player) = self.players.get(&id) else {
                continue;
            };
            let center = player.pos;
            let mut spawns = Vec::new();
            let mut states = Vec::new();
            let mut left_interest = Vec::new();
            let mut removed = Vec::new();
            let mut known = player.known_entities.clone();
            for (entity_id, view) in &views {
                if WorldVec::new(view.pos[0], view.pos[1], view.pos[2]).distance(center) > ENTITY_INTEREST_RADIUS {
                    continue;
                }
                match known.get(entity_id) {
                    None => {
                        spawns.push(view.snapshot());
                        known.insert(*entity_id, view.clone());
                    }
                    Some(previous) if previous.changed(view) => {
                        states.push(view.state_entry());
                        known.insert(*entity_id, view.clone());
                    }
                    Some(_) => {}
                }
            }
            for entity_id in known.keys().copied().collect::<Vec<u64>>() {
                if views.contains_key(&entity_id)
                    && WorldVec::new(
                        views[&entity_id].pos[0],
                        views[&entity_id].pos[1],
                        views[&entity_id].pos[2],
                    )
                    .distance(center)
                        <= ENTITY_INTEREST_RADIUS
                {
                    continue;
                }
                known.remove(&entity_id);
                if views.contains_key(&entity_id) {
                    left_interest.push(entity_id);
                } else {
                    removed.push(entity_id);
                }
            }
            if let Some(player) = self.players.get_mut(&id) {
                player.known_entities = known;
            }
            for entity in spawns {
                self.send(id, entity_spawn_message(entity));
            }
            if !states.is_empty() {
                states.sort_by_key(|state| state.id);
                self.send(id, ServerMessage::EntityState { entities: states });
            }
            if !left_interest.is_empty() {
                left_interest.sort_unstable();
                self.send(
                    id,
                    ServerMessage::EntityDespawn {
                        ids: left_interest,
                        reason: DespawnReason::LeftInterest,
                    },
                );
            }
            if !removed.is_empty() {
                removed.sort_unstable();
                self.send(
                    id,
                    ServerMessage::EntityDespawn {
                        ids: removed,
                        reason: DespawnReason::Despawned,
                    },
                );
            }
        }
    }

    /// Sends a despawn batch immediately to every player that knows the ids.
    fn broadcast_entity_despawn(&mut self, ids: &[u64], reason: DespawnReason) {
        let players: Vec<u64> = self.players.keys().copied().collect();
        for id in players {
            let known: Vec<u64> = match self.players.get_mut(&id) {
                Some(player) => {
                    let mut known = Vec::new();
                    for entity in ids {
                        if player.known_entities.remove(entity).is_some() {
                            known.push(*entity);
                        }
                    }
                    known
                }
                None => Vec::new(),
            };
            if !known.is_empty() {
                self.send(id, ServerMessage::EntityDespawn { ids: known, reason });
            }
        }
    }

    /// Periodic vitals/inventory sync: dirty states are pushed immediately,
    /// otherwise at the configured low rate.
    fn sync_players(&mut self) {
        let ids: Vec<u64> = self.players.keys().copied().collect();
        for id in ids {
            let vitals_dirty = self.host.take_network_vitals_dirty(id);
            let inventory_dirty = self.host.take_network_inventory_dirty(id);
            let vitals_due = vitals_dirty || self.tick_count.is_multiple_of(VITALS_SYNC_INTERVAL_TICKS);
            let inventory_due = inventory_dirty || self.tick_count.is_multiple_of(INVENTORY_RESYNC_INTERVAL_TICKS);
            if vitals_due {
                self.send_vitals_now(id);
            }
            if inventory_due && let Some(player) = self.host.network_player(id) {
                let message = inventory_sync_message(&self.host, player);
                self.send(id, message);
            }
        }
    }

    /// Generates and sends missing columns inside each player's interest area.
    fn stream_chunks(&mut self) {
        let ids: Vec<u64> = self.players.keys().copied().collect();
        let radius = self.config.interest_radius_chunks;
        let budget = self.config.chunks_per_tick;
        for id in ids {
            let Some(pos) = self.players.get(&id).map(|player| player.pos) else {
                continue;
            };
            self.host.ensure_streaming_around(pos, radius, budget);
            let messages = self
                .players
                .get_mut(&id)
                .map(|player| collect_chunk_messages(&self.host, player, radius, budget))
                .unwrap_or_default();
            for message in messages {
                if !self.send(id, message) {
                    break;
                }
            }
        }
    }

    /// Broadcasts one `PlayerState` per connected player at the snapshot rate.
    fn broadcast_states(&mut self) {
        if self.players.is_empty() {
            return;
        }
        let snapshots: Vec<PlayerSnapshot> = self.players.values().map(NetPlayer::snapshot).collect();
        let frames: Vec<Vec<u8>> = snapshots
            .iter()
            .filter_map(|snapshot| {
                encode_server_frame(&ServerMessage::PlayerState {
                    id: snapshot.id,
                    name: snapshot.name.clone(),
                    pos: snapshot.pos,
                    yaw: snapshot.yaw,
                    pitch: snapshot.pitch,
                })
                .ok()
            })
            .collect();
        let ids: Vec<u64> = self.players.keys().copied().collect();
        let mut failed = Vec::new();
        for id in ids {
            let mut ok = true;
            for frame in &frames {
                let queued = self
                    .players
                    .get(&id)
                    .is_some_and(|player| player.sender.try_send(frame.clone()).is_ok());
                if !queued {
                    ok = false;
                    break;
                }
            }
            if !ok {
                failed.push(id);
            }
        }
        for id in failed {
            self.disconnect(id, None, "outbound queue full");
        }
    }

    /// Saves when dirty and the configured interval has elapsed.
    fn maybe_autosave(&mut self, now: Instant) {
        let interval = self.config.save_interval_seconds;
        if interval == 0 || !self.dirty {
            return;
        }
        if now.duration_since(self.last_save) < Duration::from_secs(interval) {
            return;
        }
        match self.host.save() {
            Ok(()) => {
                self.dirty = false;
                self.last_save = now;
                log::info!("autosaved world '{}' at tick {}", self.world_name, self.tick_count);
            }
            Err(error) => {
                self.last_save = now;
                log::error!("autosave failed; previous save kept: {error}");
            }
        }
    }

    /// Encodes `message` once and queues it to every player except `except`.
    fn broadcast(&mut self, message: &ServerMessage, except: Option<u64>) {
        let Ok(frame) = encode_server_frame(message) else {
            log::error!("could not encode broadcast message");
            return;
        };
        let failed = self.queue_to_all(&frame, except);
        for id in failed {
            self.disconnect(id, None, "outbound queue full");
        }
    }

    /// Queues a pre-encoded frame to every player except `except`.
    fn queue_to_all(&self, frame: &[u8], except: Option<u64>) -> Vec<u64> {
        let mut failed = Vec::new();
        for (id, player) in &self.players {
            if Some(*id) == except {
                continue;
            }
            if player.sender.try_send(frame.to_vec()).is_err() {
                failed.push(*id);
            }
        }
        failed
    }

    /// Sends one message to one player; a full queue disconnects that player.
    fn send(&mut self, id: u64, message: ServerMessage) -> bool {
        let is_chunk = matches!(&message, ServerMessage::ChunkData { .. });
        let Ok(frame) = encode_server_frame(&message) else {
            log::error!("could not encode message for client {id}");
            return false;
        };
        let frame_len = frame.len() as u64;
        let queued = self
            .players
            .get(&id)
            .is_some_and(|player| player.sender.try_send(frame).is_ok());
        if queued && is_chunk {
            self.metrics.chunks_queued.fetch_add(1, Ordering::Relaxed);
            self.metrics.chunk_bytes_queued.fetch_add(frame_len, Ordering::Relaxed);
        }
        if !queued {
            self.disconnect(id, None, "outbound queue full");
        }
        queued
    }

    /// Removes one player, closes its window, persists its state, optionally
    /// sends a final Disconnect and tells the remaining clients with a Leave.
    fn disconnect(&mut self, id: u64, client_reason: Option<String>, log_reason: &str) {
        let Some(player) = self.players.remove(&id) else {
            return;
        };
        if player.window.is_some()
            && let Ok(frame) = encode_server_frame(&ServerMessage::ContainerClose {
                window: player.window.as_ref().map_or(0, |window| window.id),
                reason: "disconnect".to_string(),
            })
        {
            let _ = player.sender.try_send(frame);
        }
        if let Some(reason) = client_reason
            && let Ok(frame) = encode_server_frame(&ServerMessage::Disconnect {
                reason: sanitise_reason(&reason),
            })
        {
            let _ = player.sender.try_send(frame);
        }
        // Keep the player's survival state for a later reconnect by name.
        self.host.leave_network_player(id);
        self.dirty = true;
        // Unblocks a reader thread; the writer keeps its direction until the
        // queued Disconnect (if any) is flushed and the sender is dropped.
        let _ = player.stream.shutdown(Shutdown::Read);
        log::info!("player '{}' ({}) left: {log_reason}", player.name, player.id);
        if !self.players.is_empty()
            && let Ok(frame) = encode_server_frame(&ServerMessage::Leave { id })
        {
            let _ = self.queue_to_all(&frame, None);
        }
    }

    /// Disconnects everyone with the given reason; used on shutdown.
    fn close_all(&mut self, reason: &str) {
        let ids: Vec<u64> = self.players.keys().copied().collect();
        for id in ids {
            self.disconnect(id, Some(reason.to_string()), "server shutdown");
        }
        self.shutting_down = true;
    }
}

/// One replicated entity as the server currently sees it.
#[derive(Debug, Clone, PartialEq)]
struct EntityView {
    id: u64,
    kind: EntityKind,
    pos: [f64; 3],
    yaw: f32,
    health: f32,
    hurt: bool,
    stack: Option<ItemStackDto>,
}

impl EntityView {
    fn snapshot(&self) -> EntitySnapshot {
        EntitySnapshot {
            id: self.id,
            kind: self.kind.clone(),
            pos: self.pos,
            yaw: self.yaw,
            health: self.health,
            hurt: self.hurt,
        }
    }

    fn state_entry(&self) -> EntityStateEntry {
        EntityStateEntry {
            id: self.id,
            pos: self.pos,
            yaw: self.yaw,
            health: self.health,
            hurt: self.hurt,
            stack: self.stack.clone(),
        }
    }

    /// Whether a client needs a new `EntityState` for this entity.
    fn changed(&self, other: &EntityView) -> bool {
        let moved = (self.pos[0] - other.pos[0]).abs() > 0.001
            || (self.pos[1] - other.pos[1]).abs() > 0.001
            || (self.pos[2] - other.pos[2]).abs() > 0.001;
        moved
            || (self.yaw - other.yaw).abs() > 0.001
            || (self.health - other.health).abs() > 0.001
            || self.hurt != other.hurt
            || self.stack != other.stack
    }
}

/// Builds a flat `EntitySpawn` message from an entity snapshot.
fn entity_spawn_message(entity: EntitySnapshot) -> ServerMessage {
    ServerMessage::EntitySpawn {
        id: entity.id,
        kind: entity.kind,
        pos: entity.pos,
        yaw: entity.yaw,
        health: entity.health,
        hurt: entity.hurt,
    }
}

/// Converts a host slot snapshot to wire DTOs.
fn window_slot_dtos(host: &Host, slots: &[Option<ItemStack>]) -> Vec<Option<ItemStackDto>> {
    slots.iter().map(|stack| stack_dto(host.registry(), stack)).collect()
}

/// Collects chunk snapshots a player has not received yet, in square-ring order.
fn collect_chunk_messages(host: &Host, player: &mut NetPlayer, radius: i32, budget: usize) -> Vec<ServerMessage> {
    let center = BlockPos::new(
        player.pos.x.floor() as i32,
        player.pos.y.floor() as i32,
        player.pos.z.floor() as i32,
    )
    .chunk();
    let radius = radius.clamp(0, MAX_INTEREST_RADIUS_CHUNKS);
    let mut messages = Vec::new();
    'outer: for ring in 0..=radius {
        for dz in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs() != ring && dz.abs() != ring {
                    continue;
                }
                let pos = ChunkPos::new(center.x + dx, center.z + dz);
                if player.sent_chunks.contains(&pos) {
                    continue;
                }
                let Some(chunk) = host.chunk_column(pos) else {
                    continue;
                };
                match encode_chunk_checked(host.registry(), chunk) {
                    Ok(bytes) => {
                        player.sent_chunks.insert(pos);
                        messages.push(ServerMessage::ChunkData {
                            cx: pos.x,
                            cz: pos.z,
                            bytes,
                        });
                        if messages.len() >= budget {
                            break 'outer;
                        }
                    }
                    Err(error) => {
                        log::warn!("skipping oversized chunk {},{pos_z}: {error}", pos.x, pos_z = pos.z);
                        player.sent_chunks.insert(pos);
                    }
                }
            }
        }
    }
    messages
}

/// Clamps a protocol reason to a single bounded line.
fn sanitise_reason(reason: &str) -> String {
    let mut text: String = reason
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect();
    text = text.trim().to_string();
    if text.len() > MAX_REASON_LEN {
        let mut end = MAX_REASON_LEN;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    if text.is_empty() {
        "disconnected".to_string()
    } else {
        text
    }
}

/// Flat `server.toml` values. CLI flags override these after loading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerFileConfig {
    /// World directory name.
    pub world: Option<String>,
    /// World seed.
    pub seed: Option<u64>,
    /// Data directory root.
    pub data_dir: Option<PathBuf>,
    /// Listen address (`IP:port`).
    pub listen: Option<String>,
    /// Player cap.
    pub max_players: Option<u16>,
    /// MOTD.
    pub motd: Option<String>,
    /// Host streaming distance and default network interest radius.
    pub render_distance: Option<i32>,
    /// Explicit network interest radius, overriding `render_distance`.
    pub interest_radius: Option<i32>,
    /// Autosave interval in seconds.
    pub save_interval_seconds: Option<u64>,
    /// Case-insensitive player-name allow list.
    pub whitelist: Option<Vec<String>>,
    /// Default game mode for new network players (`survival`/`creative`).
    pub game_mode: Option<String>,
}

/// An error in a `server.toml` file, with a 1-based line number when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// 1-based source line, or `None` for an I/O error.
    pub line: Option<usize>,
    /// Human-readable message.
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(formatter, "line {line}: {}", self.message),
            None => write!(formatter, "{}", self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Loads a flat `server.toml` file.
pub fn load_server_config(path: &Path) -> Result<ServerFileConfig, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|error| ConfigError {
        line: None,
        message: format!("cannot read {}: {error}", path.display()),
    })?;
    parse_server_config(&text)
}

/// Parses the flat TOML subset documented in this module.
///
/// Supported keys: `world`, `seed`, `data_dir`, `listen`, `max_players`,
/// `motd`, `render_distance`, `interest_radius`, `save_interval_seconds`,
/// `whitelist` and `game_mode`. Unknown keys, malformed values and table
/// headers are errors.
pub fn parse_server_config(text: &str) -> Result<ServerFileConfig, ConfigError> {
    let mut config = ServerFileConfig::default();
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(config_error(line_number, "expected `key = value`"));
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            return Err(config_error(line_number, "empty key"));
        }
        let result = match key {
            "world" => parse_string(value).map(|value| config.world = Some(value)),
            "seed" => parse_u64(value).map(|value| config.seed = Some(value)),
            "data_dir" => parse_string(value).map(|value| config.data_dir = Some(PathBuf::from(value))),
            "listen" => parse_string(value).map(|value| config.listen = Some(value)),
            "max_players" => parse_u64(value)
                .and_then(|value| u16::try_from(value).map_err(|_| "max_players does not fit in u16".to_string()))
                .map(|value| config.max_players = Some(value)),
            "motd" => parse_string(value).map(|value| config.motd = Some(value)),
            "render_distance" => parse_i32(value).map(|value| config.render_distance = Some(value)),
            "interest_radius" => parse_i32(value).map(|value| config.interest_radius = Some(value)),
            "save_interval_seconds" => parse_u64(value).map(|value| config.save_interval_seconds = Some(value)),
            "whitelist" => parse_string_array(value).map(|value| config.whitelist = Some(value)),
            "game_mode" => parse_string(value).and_then(|value| match value.as_str() {
                "survival" | "creative" => {
                    config.game_mode = Some(value);
                    Ok(())
                }
                other => Err(format!("game_mode must be \"survival\" or \"creative\", got '{other}'")),
            }),
            other => Err(format!("unknown key '{other}'")),
        };
        if let Err(message) = result {
            return Err(config_error(line_number, &message));
        }
    }
    if let Some(0) = config.max_players {
        return Err(config_error(0, "max_players must be at least 1"));
    }
    if let Some(interval) = config.save_interval_seconds
        && interval > 86_400
    {
        return Err(config_error(0, "save_interval_seconds must be at most 86400"));
    }
    Ok(config)
}

/// Creates a configuration error with an optional line number.
fn config_error(line: usize, message: &str) -> ConfigError {
    ConfigError {
        line: (line > 0).then_some(line),
        message: message.to_string(),
    }
}

/// Removes a `#` comment that is outside a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' && delimiter == '"' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
        } else if matches!(character, '"' | '\'') {
            quote = Some(character);
        } else if character == '#' {
            return &line[..index];
        }
    }
    line
}

/// Parses a quoted string value.
fn parse_string(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err("expected a quoted string".to_string());
    }
    let delimiter = bytes[0];
    if delimiter != b'"' && delimiter != b'\'' {
        return Err("expected a quoted string".to_string());
    }
    let mut out = String::new();
    let mut chars = value[1..].char_indices();
    while let Some((offset, character)) = chars.next() {
        if character == delimiter as char {
            let rest = &value[1 + offset + character.len_utf8()..];
            if !rest.trim().is_empty() {
                return Err("unexpected characters after the quoted string".to_string());
            }
            return Ok(out);
        }
        if character == '\\' && delimiter == b'"' {
            let Some((_, escaped)) = chars.next() else {
                return Err("unterminated escape sequence".to_string());
            };
            out.push(match escaped {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '\\' => '\\',
                '"' => '"',
                other => return Err(format!("unsupported escape '\\{other}'")),
            });
        } else {
            out.push(character);
        }
    }
    Err("unterminated string".to_string())
}

/// Parses a `u64` with optional underscores.
fn parse_u64(value: &str) -> Result<u64, String> {
    value
        .replace('_', "")
        .parse::<u64>()
        .map_err(|_| format!("expected an unsigned integer, got '{value}'"))
}

/// Parses an `i32`.
fn parse_i32(value: &str) -> Result<i32, String> {
    value
        .replace('_', "")
        .parse::<i32>()
        .map_err(|_| format!("expected an integer, got '{value}'"))
}

/// Parses a string array such as `["alice", "bob"]`.
fn parse_string_array(value: &str) -> Result<Vec<String>, String> {
    let inner = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| "expected a string array".to_string())?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for element in split_array(inner)? {
        out.push(parse_string(element.trim())?);
    }
    Ok(out)
}

/// Splits array elements on commas outside quoted strings.
fn split_array(inner: &str) -> Result<Vec<&str>, String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in inner.char_indices() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' && delimiter == '"' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
        } else {
            match character {
                '"' | '\'' => quote = Some(character),
                ',' => {
                    parts.push(&inner[start..index]);
                    start = index + 1;
                }
                _ => {}
            }
        }
    }
    if quote.is_some() {
        return Err("unterminated string in array".to_string());
    }
    parts.push(&inner[start..]);
    if parts.iter().any(|part| part.trim().is_empty()) {
        return Err("empty array element".to_string());
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_config_surface() {
        let config = parse_server_config(
            r#"
            # a comment
            world = "play"
            seed = 1_234
            listen = "127.0.0.1:25565"
            max_players = 12
            motd = "hello # not a comment"
            render_distance = 8
            save_interval_seconds = 30
            whitelist = ["alice", "bob"]
            game_mode = "creative"
            "#,
        )
        .expect("valid config");
        assert_eq!(config.world.as_deref(), Some("play"));
        assert_eq!(config.seed, Some(1234));
        assert_eq!(config.listen.as_deref(), Some("127.0.0.1:25565"));
        assert_eq!(config.max_players, Some(12));
        assert_eq!(config.motd.as_deref(), Some("hello # not a comment"));
        assert_eq!(config.render_distance, Some(8));
        assert_eq!(config.save_interval_seconds, Some(30));
        assert_eq!(config.whitelist, Some(vec!["alice".to_string(), "bob".to_string()]));
        assert_eq!(config.game_mode.as_deref(), Some("creative"));
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(parse_server_config("unknown = 1").is_err());
        assert!(parse_server_config("max_players = 0").is_err());
        assert!(parse_server_config("seed = \"nope\"").is_err());
        assert!(parse_server_config("whitelist = [alice]").is_err());
        assert!(parse_server_config("[server]\nworld = \"x\"").is_err());
        assert!(parse_server_config("game_mode = \"hardcore\"").is_err());
    }

    #[test]
    fn empty_whitelist_is_allowed() {
        let config = parse_server_config("whitelist = []").expect("valid");
        assert_eq!(config.whitelist, Some(Vec::new()));
    }

    #[test]
    fn reasons_are_bounded_and_single_line() {
        assert_eq!(sanitise_reason(" hello\nworld "), "hello world");
        assert_eq!(sanitise_reason(""), "disconnected");
        assert!(sanitise_reason(&"x".repeat(MAX_REASON_LEN + 10)).len() <= MAX_REASON_LEN);
    }

    #[test]
    fn tick_window_reports_nearest_rank_percentiles_and_overruns() {
        let metrics = Metrics::new();
        for micros in 1..=1000u64 {
            metrics.record_tick(Duration::from_micros(micros));
        }
        let (count, p50, p95, p99, max) = metrics.tick_stats();
        assert_eq!(count, 1000);
        assert!((p50 - 0.500).abs() < 1e-9, "p50 {p50}");
        assert!((p95 - 0.950).abs() < 1e-9, "p95 {p95}");
        assert!((p99 - 0.990).abs() < 1e-9, "p99 {p99}");
        assert!((max - 1.000).abs() < 1e-9, "max {max}");
        assert_eq!(metrics.tick_overruns.load(Ordering::Relaxed), 0);

        metrics.record_tick(Duration::from_millis(36));
        metrics.record_tick(Duration::from_millis(51));
        assert_eq!(metrics.tick_over_envelope.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.tick_overruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn net_config_validation_rejects_out_of_range_limits() {
        let mut config = NetConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.game_mode, GameMode::Survival);
        config.max_players = 0;
        assert!(config.validate().is_err());
        config = NetConfig::default();
        config.interest_radius_chunks = MAX_INTEREST_RADIUS_CHUNKS + 1;
        assert!(config.validate().is_err());
        config = NetConfig::default();
        config.whitelist = vec!["bad name".to_string()];
        assert!(config.validate().is_err());
    }
}

//! The authoritative host shared by single-player, LAN and dedicated servers.
//!
//! The same [`Host`] drives the client's integrated server (via the client
//! crate) and the headless `openblock-server` binary. It owns the world, the
//! generator and the player state, advances a fixed 20 Hz tick, streams chunks
//! around the player and persists the world.
//!
//! Networking (`docs/spec/18-networking-multiplayer.md`) attaches to this host
//! at MS-06; v0.0.1 has no sockets.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles
#![forbid(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ob_content::Registry;
use ob_math::{BlockPos, ChunkPos, SEA_LEVEL, SectionPos, WorldVec};
use ob_sim::{BlockAction, Player, PlayerInput, TICK_DT, apply_block_actions, spawn_position, step_player};
use ob_world::World;
use ob_worldgen::Generator;

/// Configuration for a host instance.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// World seed used when a new world is created.
    pub seed: u64,
    /// World name (directory name under the data directory).
    pub world_name: String,
    /// Data directory root; `None` uses [`ob_core::data_dir`].
    pub data_dir: Option<PathBuf>,
    /// Horizontal streaming radius in chunks (default 6).
    pub render_distance_chunks: i32,
    /// Maximum chunks generated per frame/tick call.
    pub generate_budget: usize,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            world_name: "world".to_string(),
            data_dir: None,
            render_distance_chunks: 6,
            generate_budget: 2,
        }
    }
}

/// Errors from host setup and persistence.
#[derive(Debug)]
pub enum HostError {
    /// Filesystem error while loading or saving.
    Io(io::Error),
    /// The world file could not be decoded.
    Save(ob_world::SaveError),
    /// The save belongs to a newer save format.
    UnsupportedSave(u16),
    /// A player state file was malformed.
    PlayerState(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Io(err) => write!(f, "io error: {err}"),
            HostError::Save(err) => write!(f, "world save error: {err}"),
            HostError::UnsupportedSave(v) => write!(f, "save format {v} is newer than this build"),
            HostError::PlayerState(reason) => write!(f, "invalid player state: {reason}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<io::Error> for HostError {
    fn from(err: io::Error) -> Self {
        HostError::Io(err)
    }
}

impl From<ob_world::SaveError> for HostError {
    fn from(err: ob_world::SaveError) -> Self {
        HostError::Save(err)
    }
}

/// The authoritative game host.
pub struct Host {
    world: World,
    generator: Generator,
    player: Player,
    registry: Arc<Registry>,
    config: HostConfig,
    tick_count: u64,
    unloaded: Vec<ChunkPos>,
    world_dir: PathBuf,
    dirty_player: bool,
}

impl Host {
    /// Creates a host, loading the saved world if one exists at the configured path.
    pub fn open(registry: Arc<Registry>, config: HostConfig) -> Result<Self, HostError> {
        let world_dir = world_dir(&config);
        let world_path = world_dir.join("world.obs");
        let mut host;
        if world_path.exists() {
            let bytes = std::fs::read(&world_path)?;
            let world = ob_world::from_bytes(Arc::clone(&registry), &bytes)?;
            let generator = Generator::new(world.seed());
            let (player, saved_ticks) = load_player(&world_dir, &world, &generator);
            host = Self {
                world,
                generator,
                player,
                registry,
                config,
                tick_count: saved_ticks,
                unloaded: Vec::new(),
                world_dir,
                dirty_player: false,
            };
        } else {
            let generator = Generator::new(config.seed);
            let world = World::new(Arc::clone(&registry), config.seed);
            host = Self {
                world,
                generator,
                player: Player::new(WorldVec::new(0.5, SEA_LEVEL as f64 + 20.0, 0.5)),
                registry,
                config,
                tick_count: 0,
                unloaded: Vec::new(),
                world_dir,
                dirty_player: false,
            };
            host.ensure_streaming(3);
            let spawn = find_spawn(&host.world, &host.generator);
            host.player = Player::new(spawn);
            host.save()?;
        }
        host.ensure_streaming(2);
        Ok(host)
    }

    /// The world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// The world, mutably (commands/debug tools).
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// The player.
    pub fn player(&self) -> &Player {
        &self.player
    }

    /// The player, mutably.
    pub fn player_mut(&mut self) -> &mut Player {
        &mut self.player
    }

    /// The content registry.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Number of completed ticks.
    pub fn tick_count(&self) -> u64 {
        self.tick_count
    }

    /// Host configuration.
    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// Streaming radius in chunks.
    pub fn render_distance_chunks(&self) -> i32 {
        self.config.render_distance_chunks
    }

    /// Advances one simulation tick with the given player input.
    pub fn tick(&mut self, input: &PlayerInput) {
        step_player(&self.world, &mut self.player, input, TICK_DT);
        self.tick_count += 1;
        self.mark_player_dirty();
    }

    /// Applies a break/place action from the player camera.
    pub fn apply_action(&mut self, action: &BlockAction) -> bool {
        let changed = apply_block_actions(&mut self.world, &self.player, action);
        if changed {
            self.mark_player_dirty();
        }
        changed
    }

    /// Generates up to `budget` missing chunk columns around the player and
    /// unloads columns beyond the streaming radius. Returns the number of
    /// newly generated chunks.
    pub fn ensure_streaming(&mut self, budget: usize) -> usize {
        let center = self.player.pos;
        let center_chunk = BlockPos::new(
            center.x.floor() as i32,
            center.y.floor() as i32,
            center.z.floor() as i32,
        )
        .chunk();
        let radius = self.config.render_distance_chunks.max(1);
        let mut generated = 0;
        'outer: for ring in 0..=radius {
            for dz in -ring..=ring {
                for dx in -ring..=ring {
                    if dx.abs() != ring && dz.abs() != ring {
                        continue;
                    }
                    if generated >= budget {
                        break 'outer;
                    }
                    let pos = ChunkPos::new(center_chunk.x + dx, center_chunk.z + dz);
                    if self.world.chunk(pos).is_some() {
                        continue;
                    }
                    let chunk = self.generator.generate_chunk(pos, &self.registry);
                    self.world.insert_chunk(chunk);
                    generated += 1;
                }
            }
        }
        // Unload chunks outside radius + 2 (hysteresis).
        // Chebyshev hysteresis must match the square ring generation above;
        // a squared Euclidean test would immediately unload ring corners.
        let unload_radius = radius + 2;
        let mut to_unload = Vec::new();
        for (pos, _) in self.world.chunks() {
            let dx = (pos.x - center_chunk.x).abs();
            let dz = (pos.z - center_chunk.z).abs();
            if dx > unload_radius || dz > unload_radius {
                to_unload.push(*pos);
            }
        }
        for pos in to_unload {
            self.world.remove_chunk(pos);
            self.unloaded.push(pos);
        }
        generated
    }

    /// Takes the list of chunk columns unloaded since the last call (the client
    /// removes their meshes).
    pub fn take_unloaded_chunks(&mut self) -> Vec<ChunkPos> {
        std::mem::take(&mut self.unloaded)
    }

    /// Takes the sections whose meshes are stale.
    pub fn take_dirty_sections(&mut self) -> Vec<SectionPos> {
        self.world.take_dirty_sections()
    }

    /// Number of pending dirty sections (without draining them).
    pub fn dirty_sections(&self) -> usize {
        self.world.dirty_count()
    }

    /// Whether the player moved since the last save (autosave hint).
    pub fn player_dirty(&self) -> bool {
        self.dirty_player
    }

    fn mark_player_dirty(&mut self) {
        self.dirty_player = true;
    }

    /// Saves the world and player state atomically.
    pub fn save(&mut self) -> Result<(), HostError> {
        std::fs::create_dir_all(&self.world_dir)?;
        let bytes = ob_world::to_bytes(&self.world);
        write_atomic(&self.world_dir.join("world.obs"), &bytes)?;
        write_atomic(
            &self.world_dir.join("player.txt"),
            player_state(&self.player, self.tick_count).as_bytes(),
        )?;
        self.dirty_player = false;
        Ok(())
    }

    /// Deterministic hash of the simulation state (world blocks in canonical
    /// order, player position quantised and tick count). Used by the smoke test
    /// and by cross-platform determinism checks.
    pub fn state_hash(&self) -> u64 {
        let mut hasher = StableHasher::new();
        hasher.write_u64(self.world.seed());
        hasher.write_u64(self.tick_count);
        hasher.write_i64((self.player.pos.x * 1000.0) as i64);
        hasher.write_i64((self.player.pos.y * 1000.0) as i64);
        hasher.write_i64((self.player.pos.z * 1000.0) as i64);
        let mut chunks: Vec<_> = self.world.chunks().collect();
        chunks.sort_by_key(|(pos, _)| (pos.x, pos.z));
        for (pos, _chunk) in chunks {
            hasher.write_i32(pos.x);
            hasher.write_i32(pos.z);
            let (ox, oz) = pos.origin();
            for y in ob_math::WORLD_MIN_Y..ob_math::WORLD_MIN_Y + (ob_math::N_SECTIONS * 16) as i32 {
                for z in 0..16 {
                    for x in 0..16 {
                        let block = self.world.get_block(BlockPos::new(ox + x, y, oz + z));
                        hasher.write_u16(block.0);
                    }
                }
            }
        }
        hasher.finish()
    }

    /// Approximate heap memory used by the host.
    pub fn memory_bytes(&self) -> usize {
        self.world.heap_bytes()
    }
}

fn world_dir(config: &HostConfig) -> PathBuf {
    config
        .data_dir
        .clone()
        .unwrap_or_else(ob_core::data_dir)
        .join("worlds")
        .join(&config.world_name)
}

fn find_spawn(world: &World, generator: &Generator) -> WorldVec {
    for radius in 0..64i32 {
        for angle_index in 0..16 {
            let angle = angle_index as f64 * std::f64::consts::TAU / 16.0;
            let x = (angle.cos() * radius as f64 * 8.0) as i32;
            let z = (angle.sin() * radius as f64 * 8.0) as i32;
            let height = generator.height_at(x, z);
            if height > SEA_LEVEL + 2 {
                return spawn_position(world, x, z);
            }
        }
    }
    WorldVec::new(0.5, generator.height_at(0, 0) as f64 + 2.0, 0.5)
}

fn player_state(player: &Player, ticks: u64) -> String {
    format!(
        "{:.17} {:.17} {:.17} {:.9} {:.9} {} {}\n",
        player.pos.x, player.pos.y, player.pos.z, player.yaw, player.pitch, player.flying as u8, ticks
    )
}

fn load_player(world_dir: &Path, world: &World, generator: &Generator) -> (Player, u64) {
    let path = world_dir.join("player.txt");
    if let Ok(text) = std::fs::read_to_string(path) {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() == 7
            && let (Ok(x), Ok(y), Ok(z), Ok(yaw), Ok(pitch), Ok(flying), Ok(ticks)) = (
                parts[0].parse::<f64>(),
                parts[1].parse::<f64>(),
                parts[2].parse::<f64>(),
                parts[3].parse::<f32>(),
                parts[4].parse::<f32>(),
                parts[5].parse::<u8>(),
                parts[6].parse::<u64>(),
            )
        {
            let mut player = Player::new(WorldVec::new(x, y, z));
            player.yaw = yaw;
            player.pitch = pitch;
            player.flying = flying != 0;
            return (player, ticks);
        }
    }
    let spawn = find_spawn(world, generator);
    (Player::new(spawn), 0)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// FNV-1a 64-bit stable hasher for cross-platform state hashes.
struct StableHasher(u64);

impl StableHasher {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    fn write_u16(&mut self, v: u16) {
        self.write(&v.to_le_bytes());
    }
    fn write_i32(&mut self, v: i32) {
        self.write(&v.to_le_bytes());
    }
    fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
    fn write_i64(&mut self, v: i64) {
        self.write(&v.to_le_bytes());
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

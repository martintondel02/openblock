//! Loopback integration tests for the client network core.
//!
//! Starts a real [`DedicatedServer`] on `127.0.0.1:0` and drives
//! [`ob_client::net::NetworkClient`] headlessly. The v1 cases cover the creative
//! handshake, spawn chunks, movement, block edits, chat, ping and clean
//! disconnect. The protocol v2 cases exercise survival sync, authoritative
//! place/break/pickup, mob replication and attacks, furnace and chest windows
//! with revisions, damage/death/respawn, out-of-reach prediction reverts and
//! reconnect restore. No window, GPU, external network or wall-clock luck is
//! involved; every wait is a deadline and the whole suite runs in about a
//! second.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_client::containers::{ClickButton, SlotId};
use ob_client::net::{
    ConnectOptions, MAX_CHAT_SENDER_CHARS, MAX_CHAT_TEXT_CHARS, MAX_PLAYER_LIST, NetEvent, NetState, NetworkClient,
};
use ob_client::net_state::{MAX_ENTITY_BATCH, ReplicatedKind};
use ob_content::{BlockId, MobKind, Registry, SMELT_TICKS_PER_ITEM};
use ob_math::{BlockPos, ChunkPos, WorldVec};
use ob_server::{DedicatedServer, FurnaceSlot, GameMode, Host, HostConfig, ItemStack, NetConfig};
use ob_sim::BlockAction;

const WAIT: Duration = Duration::from_secs(10);

/// Test-only temporary data directory removed on drop (best effort).
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "openblock-client-net-{label}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temporary data directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Loopback {
    server: Option<DedicatedServer>,
    dir: TempDir,
}

impl Loopback {
    fn start(registry: &Arc<Registry>) -> Self {
        Self::start_with(registry, GameMode::Creative, |_, _| ()).0
    }

    /// Starts a survival server after running `setup` on the host.
    ///
    /// The setup hook runs after the spawn area is streamed, so tests can
    /// install deterministic platforms, containers or item entities before the
    /// server starts ticking.
    fn start_survival<T>(registry: &Arc<Registry>, setup: impl FnOnce(&mut Host, &Registry) -> T) -> (Self, T) {
        Self::start_with(registry, GameMode::Survival, setup)
    }

    fn start_with<T>(
        registry: &Arc<Registry>,
        game_mode: GameMode,
        setup: impl FnOnce(&mut Host, &Registry) -> T,
    ) -> (Self, T) {
        let dir = TempDir::new("loopback");
        let mut host = Host::open(
            Arc::clone(registry),
            HostConfig {
                seed: 42,
                world_name: "client-loopback".to_string(),
                data_dir: Some(dir.path().to_path_buf()),
                render_distance_chunks: 2,
                generate_budget: 8,
            },
        )
        .expect("open test world");
        for _ in 0..64 {
            if host.ensure_streaming(16) == 0 {
                break;
            }
        }
        let setup = setup(&mut host, registry);
        let server = DedicatedServer::start(
            host,
            NetConfig {
                listen: "127.0.0.1:0".parse::<SocketAddr>().expect("ephemeral loopback"),
                max_players: 4,
                motd: "client loopback".to_string(),
                interest_radius_chunks: 2,
                chunks_per_tick: 16,
                save_interval_seconds: 0,
                whitelist: Vec::new(),
                game_mode,
            },
        )
        .expect("start loopback server");
        (
            Self {
                server: Some(server),
                dir,
            },
            setup,
        )
    }

    fn addr(&self) -> SocketAddr {
        self.server.as_ref().expect("running server").local_addr()
    }

    fn server(&self) -> &DedicatedServer {
        self.server.as_ref().expect("running server")
    }

    fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.stop().expect("stop saves the world");
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            let _ = server.stop();
        }
    }
}

/// Polls and maintains chunks until `condition` is true or the deadline passes.
fn wait_for(
    client: &mut NetworkClient,
    what: &str,
    mut condition: impl FnMut(&NetworkClient) -> bool,
) -> Vec<NetEvent> {
    let deadline = Instant::now() + WAIT;
    let mut events = Vec::new();
    loop {
        events.extend(client.poll());
        client.maintain_chunks(4);
        if condition(client) {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; state {:?}, {} chunks",
            client.state(),
            client.loaded_chunks()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits for one specific `action_rejected` pair.
fn wait_for_rejection(client: &mut NetworkClient, action: &str, reason: &str) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        for event in client.poll() {
            if let NetEvent::ActionRejected {
                action: rejected_action,
                reason: rejected_reason,
                ..
            } = event
                && rejected_action == action
                && rejected_reason == reason
            {
                return true;
            }
        }
        client.maintain_chunks(4);
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn spawn_chunk(client: &NetworkClient) -> ChunkPos {
    let spawn = client.server_info().spawn;
    BlockPos::new(spawn.x.floor() as i32, spawn.y.floor() as i32, spawn.z.floor() as i32).chunk()
}

#[test]
fn loopback_handshake_chunks_move_edit_chat_ping_disconnect() {
    let registry = Arc::new(Registry::base());
    let mut loopback = Loopback::start(&registry);
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "tester",
        2,
        Arc::clone(&registry),
    ))
    .expect("connect and complete handshake");

    // Handshake metadata.
    assert_eq!(client.local_id(), 1, "first connection id");
    assert_eq!(client.server_info().world_name, "client-loopback");
    assert_eq!(client.server_info().seed, 42);
    assert_eq!(client.server_info().max_players, 4);
    assert_eq!(client.server_info().motd, "client loopback");
    assert_eq!(client.endpoint(), addr.to_string());
    assert_eq!(client.player_count(), 1);
    assert_eq!(client.player_entries()[0].name, "tester");
    assert!(client.player_entries()[0].is_self);
    // The server's spawn is a real position and the client local player starts there.
    assert_eq!(client.player().pos, client.server_info().spawn);

    // Chunk streaming and the loading threshold.
    wait_for(&mut client, "spawn chunks", |client| {
        client.is_ready() && client.world().chunk(spawn_chunk(client)).is_some()
    });
    assert!(client.state().is_playing(), "state advanced to playing");
    let (loaded, target) = client.loading_progress();
    assert!(loaded >= 5, "spawn plus neighbours loaded: {loaded}/{target}");
    assert!(target >= 9, "render radius 2 should target 25 chunks: {target}");
    assert!(client.loaded_chunks() >= 5);
    assert_eq!(client.loading_progress().1, 25);
    // Every loaded column was decoded against the local registry and its
    // neighbours were re-marked for meshing.
    let spawn = spawn_chunk(&client);
    assert!(client.world().chunk(spawn).is_some());
    // The frame-loop movement path only steps when the 3x3 collision area is
    // resident; wait for that before driving tick_player.
    let deadline = Instant::now() + WAIT;
    while !(0..=2).all(|dz| {
        (-1..=1).all(|dx| {
            client
                .world()
                .chunk(ChunkPos::new(spawn.x + dx, spawn.z + dz - 1))
                .is_some()
        })
    }) {
        assert!(Instant::now() < deadline, "3x3 spawn area did not load");
        client.poll();
        client.maintain_chunks(4);
        std::thread::sleep(Duration::from_millis(10));
    }
    let before_flight = client.player().pos;
    client.player_mut().flying = true;
    for _ in 0..10 {
        client.tick_player(ob_sim::PlayerInput {
            forward: 1.0,
            ..ob_sim::PlayerInput::default()
        });
        client.poll();
    }
    assert!(
        client.player().pos.distance(before_flight) > 0.2,
        "tick_player moved the local player"
    );
    let deadline = Instant::now() + WAIT;
    while !loopback.server().players().first().is_some_and(|player| {
        WorldVec::new(player.pos[0], player.pos[1], player.pos[2]).distance(client.server_info().spawn) > 0.2
    }) {
        assert!(Instant::now() < deadline, "server did not receive the tick_player Move");
        client.poll();
        std::thread::sleep(Duration::from_millis(10));
    }

    // Ping / pong.
    assert!(client.send_ping());
    wait_for(&mut client, "pong", |client| client.ping_ms().is_some());
    assert!(client.ping_ms().is_some(), "ping measured");

    // Client-authoritative movement is accepted and echoed in PlayerState.
    // Movement via the explicit test API bypasses the change check and rate
    // limit; the frame loop uses tick_player (exercised above).
    let start = client.player().pos;
    let target_pos = [start.x + 1.0, start.y, start.z];
    assert!(client.send_move_now(target_pos, 0.75, -0.2));
    let mut events = wait_for(&mut client, "own PlayerState echo", |client| {
        client
            .last_server_state()
            .is_some_and(|snapshot| (snapshot.pos[0] - target_pos[0]).abs() < 1e-6)
    });
    let echoed = loopback.server().players();
    assert_eq!(echoed.len(), 1);
    assert!(
        (echoed[0].pos[0] - target_pos[0]).abs() < 1e-6,
        "server stored the moved position: {:?}",
        echoed[0].pos
    );
    assert!((echoed[0].yaw - 0.75).abs() < 1e-6);
    assert!((echoed[0].pitch + 0.2).abs() < 1e-6);
    assert!(
        client.avatar_states(Instant::now()).is_empty(),
        "the local player's own PlayerState must not become a remote avatar"
    );
    events.extend(client.poll());
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, NetEvent::Disconnected { .. }))
    );

    // Break a real block under the player with local prediction, then wait for
    // the server's authoritative broadcast and verify its world changed.
    let eye = client.player().eye();
    let hit = client
        .world()
        .raycast(eye, ob_math::WorldVec::new(0.0, -1.0, 0.0), 12.0)
        .expect("ground below the spawn");
    let block_pos = hit.block;
    let original = client.world().get_block(block_pos);
    assert_ne!(original, BlockId::AIR);
    assert!(client.edit_block(block_pos, BlockId::AIR), "break predicted and queued");
    assert_eq!(client.world().get_block(block_pos), BlockId::AIR, "prediction applied");
    wait_for(&mut client, "server break broadcast", |_| {
        loopback.server().block_at(block_pos) == Some(BlockId::AIR) && loopback.server().edit_counts().0 >= 1
    });
    assert_eq!(loopback.server().block_at(block_pos), Some(BlockId::AIR));
    // Put it back through the same path; the server accepts air -> solid.
    assert!(client.edit_block(block_pos, original), "place predicted and queued");
    wait_for(&mut client, "server place broadcast", |_| {
        loopback.server().block_at(block_pos) == Some(original) && loopback.server().edit_counts().0 >= 2
    });
    assert_eq!(loopback.server().block_at(block_pos), Some(original));

    // Chat is relayed back to the sender with the registered name.
    assert!(client.send_chat("hello from the loopback test").is_ok());
    let chat = wait_for(&mut client, "chat relay", |client| {
        client.chat_log().lines().any(|line| line.from == "tester")
    });
    assert!(
        chat.iter().any(|event| matches!(
            event,
            NetEvent::Chat { from, text } if from == "tester" && text == "hello from the loopback test"
        )),
        "chat event: {chat:?}"
    );
    assert!(
        client
            .chat_log()
            .lines()
            .any(|line| line.from == "tester" && line.text == "hello from the loopback test")
    );

    // Clean disconnect: the server observes the player leaving.
    client.shutdown();
    assert!(matches!(client.state(), NetState::Disconnected(_)));
    assert!(!client.is_connected());
    let deadline = Instant::now() + WAIT;
    while loopback.server().player_count() != 0 {
        assert!(Instant::now() < deadline, "server did not observe the clean disconnect");
        std::thread::sleep(Duration::from_millis(10));
    }
    loopback.stop();
    drop(client);
    assert!(loopback.dir.path().exists(), "temp world removed only by TempDir drop");
}

#[test]
fn second_client_roster_chat_and_leave_are_replicated() {
    let registry = Arc::new(Registry::base());
    let mut loopback = Loopback::start(&registry);
    let addr = loopback.addr();
    let mut alice = NetworkClient::connect(ConnectOptions::new(addr.to_string(), "alice", 2, Arc::clone(&registry)))
        .expect("alice connects");
    wait_for(&mut alice, "alice spawn chunks", NetworkClient::is_ready);
    assert_eq!(alice.player_count(), 1);

    let mut bob = NetworkClient::connect(ConnectOptions::new(addr.to_string(), "bob", 2, Arc::clone(&registry)))
        .expect("bob connects");
    wait_for(&mut bob, "bob spawn chunks", NetworkClient::is_ready);
    assert_eq!(bob.player_count(), 2, "bob sees the roster including alice");

    // Alice learns about Bob through Join + PlayerState and can produce avatar
    // states for the renderer.
    let events = wait_for(&mut alice, "bob's join", |alice| alice.player_count() == 2);
    let entries = alice.player_entries();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|entry| entry.name == "bob" && !entry.is_self));
    assert!(entries.iter().any(|entry| entry.name == "alice" && entry.is_self));
    let avatars = alice.avatar_states(Instant::now());
    assert_eq!(avatars.len(), 1, "one remote avatar");
    assert_eq!(avatars[0].name, "bob");
    let colour = ob_client::net::avatar_color(avatars[0].id, &avatars[0].name);
    assert!(colour.iter().all(|channel| (0.0..=1.0).contains(channel)));

    // Bob's chat is relayed to Alice.
    bob.send_chat("hello alice").expect("bob chat");
    wait_for(&mut alice, "bob's chat", |alice| {
        alice.chat_log().lines().any(|line| line.from == "bob")
    });
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NetEvent::Joined { name, .. } if name == "bob")),
        "join event: {events:?}"
    );

    // Bob leaves cleanly; Alice receives Leave and drops the remote avatar.
    bob.shutdown();
    let left_events = wait_for(&mut alice, "bob's leave", |alice| alice.player_count() == 1);
    assert!(
        left_events
            .iter()
            .any(|event| matches!(event, NetEvent::Left { name, .. } if name == "bob")),
        "leave event: {left_events:?}"
    );
    assert!(alice.avatar_states(Instant::now()).is_empty());

    alice.shutdown();
    loopback.stop();
}

#[test]
fn server_shutdown_produces_a_reasoned_disconnect() {
    let registry = Arc::new(Registry::base());
    let mut loopback = Loopback::start(&registry);
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "leaver",
        2,
        Arc::clone(&registry),
    ))
    .expect("connect");
    wait_for(&mut client, "spawn chunks", NetworkClient::is_ready);
    loopback.server().request_shutdown();
    let deadline = Instant::now() + WAIT;
    let mut reason = None;
    while Instant::now() < deadline {
        for event in client.poll() {
            if let NetEvent::Disconnected { reason: disconnected } = event {
                reason = Some(disconnected);
            }
        }
        if reason.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let reason = reason.expect("server shutdown disconnect event");
    assert!(
        reason.contains("server_shutdown"),
        "expected server_shutdown, got {reason:?}"
    );
    assert!(matches!(client.state(), NetState::Disconnected(_)));
    client.shutdown();
    loopback.stop();
}

/// Item stack helper for setup and assertions.
fn item(registry: &Registry, name: &str, count: u8) -> ItemStack {
    ItemStack {
        item: registry.item_id(name).expect("known item"),
        count,
        durability: None,
    }
}

/// Clears a 3x3x5 volume around `base` and installs a stone support below it.
fn clear_platform(host: &mut Host, registry: &Registry, base: BlockPos) {
    let stone = registry.id("openblock:stone").expect("stone");
    for dy in -1..=3 {
        for dx in -1..=1 {
            for dz in -1..=1 {
                host.world_mut().set_block(base.offset(dx, dy, dz), BlockId::AIR);
            }
        }
    }
    assert!(
        host.world_mut().set_block(base.offset(0, -1, 0), stone),
        "support cell must be in a loaded chunk"
    );
}

/// Installs one furnace or chest through the real creative placement path.
fn install_container(host: &mut Host, registry: &Registry, block: &str, offset_z: i32) -> BlockPos {
    let spawn = host.player().pos;
    let base = BlockPos::new(
        spawn.x.floor() as i32 + 1,
        spawn.y.floor() as i32 + 3,
        spawn.z.floor() as i32 + offset_z,
    );
    clear_platform(host, registry, base);
    let previous = host.player().clone();
    host.player_mut().pos = WorldVec::new(base.x as f64 + 0.5, base.y as f64 + 3.0, base.z as f64 + 0.5);
    host.player_mut().yaw = 0.0;
    host.player_mut().pitch = -std::f32::consts::FRAC_PI_2;
    host.set_game_mode(GameMode::Creative);
    let action = BlockAction {
        break_block: false,
        place_block: true,
        place: registry.id(block).expect("container block"),
    };
    let outcome = host.apply_action_with_outcome(&action);
    *host.player_mut() = previous;
    host.set_game_mode(GameMode::Survival);
    assert!(outcome.changed, "creative placement must succeed for {block}");
    assert_eq!(
        host.world().get_block(base),
        registry.id(block).expect("container block"),
        "container block installed at {base:?}"
    );
    base
}

fn spawn_chunk_pos(client: &NetworkClient) -> ChunkPos {
    let spawn = client.server_info().spawn;
    BlockPos::new(spawn.x.floor() as i32, spawn.y.floor() as i32, spawn.z.floor() as i32).chunk()
}

/// Polls until the spawn area and its 3x3 collision neighbourhood are resident.
fn wait_ready(client: &mut NetworkClient) {
    wait_for(client, "spawn chunks", |client| client.is_ready());
    let spawn = spawn_chunk_pos(client);
    let deadline = Instant::now() + WAIT;
    while !(0..=2).all(|dz| {
        (-1..=1).all(|dx| {
            client
                .world()
                .chunk(ChunkPos::new(spawn.x + dx, spawn.z + dz))
                .is_some()
        })
    }) {
        assert!(Instant::now() < deadline, "3x3 spawn area did not load");
        client.poll();
        client.maintain_chunks(4);
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until the authoritative survival state and entity snapshot arrived.
fn wait_survival_sync(client: &mut NetworkClient) {
    wait_for(client, "initial survival sync", |client| {
        client.survival().vitals_received && client.survival().inventory_received
    });
}

#[test]
fn survival_handshake_syncs_vitals_inventory_and_selection() {
    let registry = Arc::new(Registry::base());
    let (mut loopback, ()) = Loopback::start_survival(&registry, |_, _| ());
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "survivor",
        2,
        Arc::clone(&registry),
    ))
    .expect("connect survival server");
    wait_ready(&mut client);
    wait_survival_sync(&mut client);

    assert!((client.health() - 20.0).abs() < 0.01, "health {}", client.health());
    assert!((client.hunger() - 20.0).abs() < 0.01, "hunger {}", client.hunger());
    assert_eq!(client.armor_defense(), 0);
    assert_eq!(client.selected_slot(), 0);
    assert!(!client.is_dead());
    assert!(client.inventory().iter().all(Option::is_none));
    assert!(client.equipment().iter().all(Option::is_none));
    assert_eq!(client.server_info().world_name, "client-loopback");
    assert_eq!(client.entities().mob_count(), 0);
    assert_eq!(client.entities().item_count(), 0);

    // The selected hotbar slot is server-authoritative: selecting echoes back
    // through `selected_slot` before the HUD highlight changes.
    assert!(client.select_slot(4));
    wait_for(&mut client, "selected slot echo", |client| client.selected_slot() == 4);
    assert!(client.select_slot(0));
    wait_for(&mut client, "selected slot echo 0", |client| {
        client.selected_slot() == 0
    });

    client.shutdown();
    loopback.stop();
}

#[test]
fn survival_place_break_pickup_mirrors_authoritative_inventory() {
    let registry = Arc::new(Registry::base());
    let (mut loopback, base) = Loopback::start_survival(&registry, |host, registry| {
        let spawn = host.player().pos;
        let base = BlockPos::new(
            spawn.x.floor() as i32 + 1,
            spawn.y.floor() as i32,
            spawn.z.floor() as i32,
        );
        clear_platform(host, registry, base);
        base
    });
    let dirt = registry.id("openblock:dirt").expect("dirt");
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "builder",
        2,
        Arc::clone(&registry),
    ))
    .expect("connect survival server");
    wait_ready(&mut client);
    wait_survival_sync(&mut client);
    assert_eq!(client.world().get_block(base), BlockId::AIR);
    assert!(
        base.center().distance(client.player().pos) <= 7.0,
        "arena is within the server's reach check"
    );

    // Give an authoritative stack and select its slot.
    assert!(
        loopback
            .server()
            .give_network_item(client.local_id(), "openblock:dirt", 4)
    );
    wait_for(&mut client, "dirt inventory sync", |client| {
        client
            .inventory()
            .iter()
            .flatten()
            .any(|stack| stack.item == registry.item_id("openblock:dirt").expect("dirt item") && stack.count == 4)
    });
    let slot = client
        .inventory()
        .iter()
        .position(|stack| {
            stack.is_some_and(|stack| stack.item == registry.item_id("openblock:dirt").expect("dirt item"))
        })
        .expect("dirt slot");
    assert!(client.select_slot(slot));
    wait_for(&mut client, "dirt selected", |client| client.selected_slot() == slot);

    // Placement is predicted locally but the item count changes only when the
    // server's InventorySync arrives.
    assert!(client.edit_block(base, dirt), "placement intent queued");
    assert_eq!(client.world().get_block(base), dirt, "placement predicted");
    wait_for(&mut client, "server placement", |_| {
        loopback.server().block_at(base) == Some(dirt)
    });
    wait_for(&mut client, "placement consumed one", |client| {
        client
            .inventory()
            .iter()
            .flatten()
            .any(|stack| stack.item == registry.item_id("openblock:dirt").expect("dirt item") && stack.count == 3)
    });

    // Breaking spawns a real item entity. Item ids carry the server's high bit
    // and must be treated as opaque.
    assert!(client.edit_block(base, BlockId::AIR), "break intent queued");
    wait_for(&mut client, "server break", |_| {
        loopback.server().block_at(base) == Some(BlockId::AIR)
    });
    let events = wait_for(&mut client, "drop item entity", |client| {
        client.entities().item_count() >= 1
    });
    let _ = events;
    let item_id = client
        .entities()
        .ids()
        .into_iter()
        .find(|id| matches!(client.entities().kind(*id), Some(ReplicatedKind::Item(_))))
        .expect("replicated drop");
    assert!(item_id & (1 << 63) != 0, "wire item ids use the high bit on the server");
    assert!(matches!(
        client.entities().kind(item_id),
        Some(ReplicatedKind::Item(stack)) if stack.count == 1
    ));

    // Walk to the drop so the authoritative pickup path collects it, then wait
    // for both the despawn and the inventory resync.
    let pos = [base.x as f64 + 0.5, client.player().pos.y, base.z as f64 + 0.5];
    let yaw = client.player().yaw;
    let pitch = client.player().pitch;
    assert!(client.send_move_now(pos, yaw, pitch));
    wait_for(&mut client, "item pickup inventory sync", |client| {
        client
            .inventory()
            .iter()
            .flatten()
            .any(|stack| stack.item == registry.item_id("openblock:dirt").expect("dirt item") && stack.count == 4)
    });
    assert_eq!(client.entities().item_count(), 0, "picked-up entity despawned");

    client.shutdown();
    loopback.stop();
}

#[test]
fn survival_mob_spawn_state_and_kill_by_attack() {
    let registry = Arc::new(Registry::base());
    let (mut loopback, ()) = Loopback::start_survival(&registry, |_, _| ());
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "hunter",
        2,
        Arc::clone(&registry),
    ))
    .expect("connect survival server");
    wait_ready(&mut client);
    wait_survival_sync(&mut client);
    let player = client.player().pos;
    let offsets = [(2.0, 0.0), (0.0, 2.0), (-2.0, 0.0), (0.0, -2.0), (1.5, 1.5)];
    let mut spawned = None;
    for (dx, dz) in offsets {
        let pos = [player.x + dx, player.y, player.z + dz];
        if let Some(id) = loopback.server().spawn_mob(MobKind::Chicken, pos) {
            spawned = Some(id);
            break;
        }
    }
    let id = spawned.expect("spawn a chicken near the player");
    wait_for(&mut client, "mob spawn replication", |client| {
        matches!(client.entities().kind(id), Some(ReplicatedKind::Mob(MobKind::Chicken)))
    });

    // First hit: expect attack feedback and a hurt/health state update.
    assert!(client.send_attack(Some(id)));
    let events = wait_for(&mut client, "first attack result", |client| {
        client.entities().health(id).is_some_and(|health| health < 4.0)
    });
    assert!(
        events.iter().any(|event| matches!(
            event,
            NetEvent::AttackResult { target, hit: true, .. } if *target == id
        )),
        "attack result event: {events:?}"
    );
    assert_eq!(client.entities().hurt(id), Some(true));

    // Finish the kill. Rate limits allow the whole burst; the server broadcasts
    // `entity_despawn { reason: killed }`.
    for _ in 0..4 {
        assert!(client.send_attack(Some(id)));
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_for(&mut client, "mob killed despawn", |client| {
        client.entities().kind(id).is_none()
    });
    assert!(client.entities().mob_count() == 0);
    assert_eq!(
        client.entities().last_despawn().map(|(reason, _)| reason),
        Some(ob_protocol::DespawnReason::Killed)
    );

    client.shutdown();
    loopback.stop();
}

#[test]
fn survival_furnace_open_put_smelt_take_and_stale_revision() {
    let registry = Arc::new(Registry::base());
    let (mut loopback, furnace_pos) = Loopback::start_survival(&registry, |host, registry| {
        let pos = install_container(host, registry, "openblock:furnace", 0);
        host.put_furnace_slot(pos, FurnaceSlot::Input, item(registry, "openblock:raw_iron", 1));
        host.put_furnace_slot(pos, FurnaceSlot::Fuel, item(registry, "openblock:coal", 1));
        let state = host.furnace_at_mut(pos).expect("furnace state");
        state.cook_progress = SMELT_TICKS_PER_ITEM.saturating_sub(4);
        state.burn_remaining = 100;
        state.burn_total = 100;
        pos
    });
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(addr.to_string(), "smith", 2, Arc::clone(&registry)))
        .expect("connect survival server");
    wait_ready(&mut client);
    wait_survival_sync(&mut client);

    assert!(client.use_block(furnace_pos));
    wait_for(&mut client, "furnace window", NetworkClient::container_open);
    let window = client.containers().window().expect("open window");
    assert_eq!(window.kind, ob_protocol::ContainerKind::Furnace);
    assert_eq!(window.ui_kind(), ob_client::containers::ContainerKind::Furnace);
    assert_eq!(window.slots.len(), 3);
    let revision = window.revision;

    // A stale content revision is rejected and never applied.
    assert!(client.send_inventory_move(100, 0, 1, Some(revision + 50)));
    assert!(
        wait_for_rejection(&mut client, "inventory_move", "stale_revision"),
        "stale revision must be rejected"
    );
    assert!(
        client
            .containers()
            .revision()
            .is_some_and(|current| current >= revision)
    );

    // The near-complete cook produces an output stack, mirrored by a
    // container_update without a chest.
    wait_for(&mut client, "smelted output", |client| {
        client
            .containers()
            .window()
            .and_then(|window| window.stack(2))
            .is_some()
    });
    assert_eq!(
        client
            .containers()
            .window()
            .and_then(|window| window.stack(2))
            .map(|stack| stack.count),
        Some(1)
    );

    // Put one coal from the hotbar into the fuel slot through the real intent.
    assert!(
        loopback
            .server()
            .give_network_item(client.local_id(), "openblock:coal", 2)
    );
    let coal_item = registry.item_id("openblock:coal").expect("coal item");
    wait_for(&mut client, "coal inventory sync", |client| {
        client
            .inventory()
            .iter()
            .flatten()
            .any(|stack| stack.item == coal_item && stack.count == 2)
    });
    let coal_slot = client
        .inventory()
        .iter()
        .position(|stack| stack.is_some_and(|stack| stack.item == coal_item))
        .expect("coal slot");
    let revision = client.containers().revision().expect("revision");
    assert!(client.send_inventory_move(coal_slot as u16, 101, 1, Some(revision)));
    wait_for(&mut client, "fuel slot updated", |client| {
        client
            .containers()
            .window()
            .is_some_and(|window| window.revision > revision)
    });

    // Take the output with the cursor preview planner and an empty hotbar slot.
    let empty = client
        .inventory()
        .iter()
        .position(Option::is_none)
        .expect("empty hotbar slot");
    let outcome = client.container_click(SlotId::FurnaceOutput, ClickButton::Left);
    assert!(outcome.transfer.is_none() && outcome.message.is_none());
    assert_eq!(client.containers().cursor().map(|cursor| cursor.source), Some(102));
    let outcome = client.container_click(SlotId::Hotbar(empty), ClickButton::Left);
    assert!(outcome.transfer.is_some(), "output take intent: {outcome:?}");
    wait_for(&mut client, "output in inventory", |client| {
        client
            .inventory()
            .iter()
            .flatten()
            .any(|stack| stack.item == registry.item_id("openblock:iron_ingot").expect("iron ingot"))
    });
    wait_for(&mut client, "output slot emptied", |client| {
        client
            .containers()
            .window()
            .is_some_and(|window| window.stack(2).is_none())
    });

    assert!(client.send_container_close());
    assert!(!client.container_open());
    client.shutdown();
    loopback.stop();
}

#[test]
fn survival_chest_transfer_revision_and_invalid_slot() {
    let registry = Arc::new(Registry::base());
    let (mut loopback, chest_pos) = Loopback::start_survival(&registry, |host, registry| {
        let pos = install_container(host, registry, "openblock:chest", 2);
        host.put_chest_slot(pos, 0, item(registry, "openblock:stone", 6));
        pos
    });
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "looter",
        2,
        Arc::clone(&registry),
    ))
    .expect("connect survival server");
    wait_ready(&mut client);
    wait_survival_sync(&mut client);
    assert!(client.use_block(chest_pos));
    wait_for(&mut client, "chest window", NetworkClient::container_open);
    let first_revision = client.containers().revision().expect("revision");
    assert_eq!(
        client
            .containers()
            .window()
            .and_then(|window| window.stack(0))
            .map(|stack| stack.count),
        Some(6)
    );

    // Take the whole stack into the cursor, then place it into an empty hotbar
    // slot: exactly one `inventory_move` with the latest revision.
    let empty = client
        .inventory()
        .iter()
        .position(Option::is_none)
        .expect("empty hotbar slot");
    let outcome = client.container_click(SlotId::Chest(0), ClickButton::Left);
    assert!(outcome.transfer.is_none());
    let outcome = client.container_click(SlotId::Hotbar(empty), ClickButton::Left);
    let transfer = outcome.transfer.expect("chest transfer intent");
    assert_eq!((transfer.from, transfer.to, transfer.count), (100, empty as u16, 6));
    assert_eq!(transfer.revision, Some(first_revision));
    // A second click before the authoritative update must not duplicate the
    // move; the client waits for the server's sync.
    let blocked = client.container_click(SlotId::Chest(0), ClickButton::Left);
    assert!(blocked.transfer.is_none());
    assert!(blocked.message.is_some());
    wait_for(&mut client, "chest stack in inventory", |client| {
        client.inventory()[empty].is_some_and(|stack| stack.count == 6)
    });
    wait_for(&mut client, "chest revision advanced", |client| {
        client
            .containers()
            .revision()
            .is_some_and(|revision| revision > first_revision)
    });
    assert!(
        client
            .containers()
            .window()
            .is_some_and(|window| window.stack(0).is_none())
    );

    // Put two back through an explicit intent.
    let revision = client.containers().revision().expect("revision");
    assert!(client.send_inventory_move(empty as u16, 100, 2, Some(revision)));
    wait_for(&mut client, "chest slot resync", |client| {
        client
            .containers()
            .window()
            .is_some_and(|window| window.stack(0).is_some_and(|stack| stack.count == 2))
    });
    assert_eq!(client.inventory()[empty].map(|stack| stack.count), Some(4));

    // Invalid addresses and stale revisions are surfaced as action rejections.
    assert!(client.send_inventory_move(empty as u16, 200, 1, None));
    assert!(
        wait_for_rejection(&mut client, "inventory_move", "invalid_slot"),
        "out-of-range window slot must be rejected"
    );
    assert!(client.send_inventory_move(empty as u16, 100, 1, Some(u64::MAX)));
    assert!(
        wait_for_rejection(&mut client, "inventory_move", "stale_revision"),
        "stale revision must be rejected"
    );

    client.shutdown();
    loopback.stop();
}

#[test]
fn survival_damage_death_respawn_keeps_inventory() {
    let registry = Arc::new(Registry::base());
    let (mut loopback, ()) = Loopback::start_survival(&registry, |_, _| ());
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "mortal",
        2,
        Arc::clone(&registry),
    ))
    .expect("connect survival server");
    wait_ready(&mut client);
    wait_survival_sync(&mut client);
    assert!(
        loopback
            .server()
            .give_network_item(client.local_id(), "openblock:apple", 2)
    );
    let apple = registry.item_id("openblock:apple").expect("apple");
    wait_for(&mut client, "apple inventory", |client| {
        client
            .inventory()
            .iter()
            .flatten()
            .any(|stack| stack.item == apple && stack.count == 2)
    });

    // The admin damage path routes through armor and relays damage + death.
    assert!(loopback.server().damage_network_player(client.local_id(), 20.0));
    let events = wait_for(&mut client, "death", NetworkClient::is_dead);
    assert!(
        events.iter().any(|event| matches!(event, NetEvent::Damage { .. })),
        "damage event: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(event, NetEvent::Death { .. })),
        "death event: {events:?}"
    );
    assert_eq!(client.health(), 0.0);
    assert!(client.death_cause().is_some());
    // Actions are refused while dead (the server would ignore them too).
    assert!(!client.send_attack(None));
    assert!(!client.select_slot(1));
    assert!(!client.edit_block(
        BlockPos::new(
            client.player().pos.x.floor() as i32 + 1,
            client.player().pos.y.floor() as i32,
            client.player().pos.z.floor() as i32
        ),
        BlockId::AIR
    ));
    // The respawn intent is accepted once and the server restores vitals at
    // the spawn point while keeping the inventory.
    assert!(client.send_respawn());
    wait_for(&mut client, "respawn", |client| !client.is_dead());
    assert!((client.health() - 20.0).abs() < 0.01, "health {}", client.health());
    assert!((client.hunger() - 20.0).abs() < 0.01, "hunger {}", client.hunger());
    assert_eq!(client.player().pos, client.server_info().spawn);
    assert!(
        client
            .inventory()
            .iter()
            .flatten()
            .any(|stack| stack.item == apple && stack.count == 2)
    );
    assert!(!client.send_respawn(), "respawn is only valid while dead");

    client.shutdown();
    loopback.stop();
}

#[test]
fn survival_reconnect_restores_saved_player() {
    let registry = Arc::new(Registry::base());
    let (mut loopback, ()) = Loopback::start_survival(&registry, |_, _| ());
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "restorer",
        2,
        Arc::clone(&registry),
    ))
    .expect("first connect");
    wait_ready(&mut client);
    wait_survival_sync(&mut client);
    assert!(
        loopback
            .server()
            .give_network_item(client.local_id(), "openblock:apple", 3)
    );
    let apple = registry.item_id("openblock:apple").expect("apple");
    wait_for(&mut client, "saved inventory", |client| {
        client
            .inventory()
            .iter()
            .flatten()
            .any(|stack| stack.item == apple && stack.count == 3)
    });
    let spawn = client.player().pos;
    std::thread::sleep(Duration::from_millis(120));
    assert!(client.send_move_now([spawn.x + 1.5, spawn.y, spawn.z], 0.5, 0.1));
    wait_for(&mut client, "moved position stored", |_| {
        loopback
            .server()
            .players()
            .first()
            .is_some_and(|player| (player.pos[0] - (spawn.x + 1.5)).abs() < 1e-6)
    });
    let moved = loopback.server().players()[0].pos;
    client.shutdown();
    let deadline = Instant::now() + WAIT;
    while loopback.server().player_count() != 0 {
        assert!(Instant::now() < deadline, "server did not observe disconnect");
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut back = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "restorer",
        2,
        Arc::clone(&registry),
    ))
    .expect("reconnect by name");
    wait_ready(&mut back);
    wait_survival_sync(&mut back);
    assert!(
        back.inventory()
            .iter()
            .flatten()
            .any(|stack| stack.item == apple && stack.count == 3)
    );
    assert!((back.player().pos.x - moved[0]).abs() < 1e-6);
    assert!((back.player().pos.z - moved[2]).abs() < 1e-6);

    back.shutdown();
    loopback.stop();
}

#[test]
fn survival_reconnects_after_a_dead_record_and_waits_for_respawn() {
    let registry = Arc::new(Registry::base());
    let (mut loopback, ()) = Loopback::start_survival(&registry, |_, _| ());
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(addr.to_string(), "ghost", 2, Arc::clone(&registry)))
        .expect("first connect");
    wait_ready(&mut client);
    wait_survival_sync(&mut client);
    assert!(loopback.server().damage_network_player(client.local_id(), 20.0));
    wait_for(&mut client, "death before disconnect", NetworkClient::is_dead);
    client.shutdown();
    let deadline = Instant::now() + WAIT;
    while loopback.server().player_count() != 0 {
        assert!(Instant::now() < deadline, "server did not observe disconnect");
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut back = NetworkClient::connect(ConnectOptions::new(addr.to_string(), "ghost", 2, Arc::clone(&registry)))
        .expect("reconnect while dead");
    wait_ready(&mut back);
    wait_for(&mut back, "death restored on join", NetworkClient::is_dead);
    assert!(back.death_cause().is_some());
    assert!(back.send_respawn());
    wait_for(&mut back, "respawn after reconnect", |client| !client.is_dead());

    back.shutdown();
    loopback.stop();
}

#[test]
fn survival_out_of_reach_edit_reverts_prediction() {
    let registry = Arc::new(Registry::base());
    let (mut loopback, ()) = Loopback::start_survival(&registry, |_, _| ());
    let addr = loopback.addr();
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "reacher",
        2,
        Arc::clone(&registry),
    ))
    .expect("connect survival server");
    wait_ready(&mut client);
    wait_survival_sync(&mut client);
    let dirt = registry.id("openblock:dirt").expect("dirt");
    assert!(
        loopback
            .server()
            .give_network_item(client.local_id(), "openblock:dirt", 2)
    );
    let dirt_item = registry.item_id("openblock:dirt").expect("dirt item");
    wait_for(&mut client, "dirt stack", |client| {
        client.inventory().iter().flatten().any(|stack| stack.item == dirt_item)
    });
    let slot = client
        .inventory()
        .iter()
        .position(|stack| stack.is_some_and(|stack| stack.item == dirt_item))
        .expect("dirt slot");
    assert!(client.select_slot(slot));
    wait_for(&mut client, "dirt selected", |client| client.selected_slot() == slot);

    let origin = [
        client.player().pos.x.floor() as i32,
        client.player().pos.y.floor() as i32,
        client.player().pos.z.floor() as i32,
    ];
    let mut far = None;
    'search: for dx in -14..=14 {
        for dz in -14..=14 {
            let distance = (f64::from(dx * dx + dz * dz)).sqrt();
            if !(8.5..=13.0).contains(&distance) {
                continue;
            }
            for dy in -6..=6 {
                let pos = BlockPos::new(origin[0] + dx, origin[1] + dy, origin[2] + dz);
                if !pos.in_world() || client.world().chunk(pos.chunk()).is_none() {
                    continue;
                }
                let block = client.world().get_block(pos);
                if block == BlockId::AIR
                    || !registry.block(block).breakable()
                    || client.world().get_block(pos.offset(0, 1, 0)) != BlockId::AIR
                {
                    continue;
                }
                if pos.center().distance(client.player().pos) > 7.0 {
                    far = Some((pos, block));
                    break 'search;
                }
            }
        }
    }
    let (far, far_block) = far.expect("a far breakable block with air above");
    // A break prediction is applied locally, then the server rejects the reach
    // check and the client reverts the visual change.
    assert!(client.edit_block(far, BlockId::AIR));
    assert_eq!(client.world().get_block(far), BlockId::AIR);
    assert_eq!(client.pending_predictions(), 1);
    assert!(
        wait_for_rejection(&mut client, "block_edit", "out_of_reach"),
        "far break must be rejected"
    );
    assert_eq!(client.world().get_block(far), far_block, "prediction reverted");
    assert_eq!(client.pending_predictions(), 0);
    assert_eq!(loopback.server().block_at(far), Some(far_block));

    // The same applies to a placement prediction in the air above it.
    let target = far.offset(0, 1, 0);
    assert_eq!(client.world().get_block(target), BlockId::AIR);
    assert!(client.edit_block(target, dirt));
    assert_eq!(client.world().get_block(target), dirt);
    assert!(
        wait_for_rejection(&mut client, "block_edit", "out_of_reach"),
        "far placement must be rejected"
    );
    assert_eq!(client.world().get_block(target), BlockId::AIR);
    assert_eq!(client.pending_predictions(), 0);
    assert_eq!(loopback.server().block_at(target), Some(BlockId::AIR));
    assert_eq!(
        client.selected_stack().map(|stack| stack.count),
        Some(2),
        "no item consumed"
    );

    client.shutdown();
    loopback.stop();
}

/// Starts a one-client scripted server that completes the v1 handshake and then
/// hands the test a raw socket for adversarial post-handshake frames.
fn start_scripted_server() -> (SocketAddr, mpsc::Receiver<TcpStream>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted server");
    let addr = listener.local_addr().expect("scripted server address");
    let (control_tx, control_rx) = mpsc::channel();
    let thread = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept scripted client");
        socket.set_nodelay(true).expect("nodelay");
        let hello: ob_protocol::ClientMessage =
            ob_protocol::read_message(&mut socket, ob_protocol::MAX_FRAME_LEN).expect("read client hello");
        assert!(matches!(hello, ob_protocol::ClientMessage::Hello { .. }));
        let registry = Registry::base();
        let local = ob_protocol::PlayerSnapshot {
            id: 7,
            name: "tester".to_string(),
            pos: [10.0, 70.0, 10.0],
            yaw: 0.0,
            pitch: 0.0,
        };
        let handshake = [
            ob_protocol::ServerMessage::Hello {
                protocol: ob_protocol::PROTOCOL_VERSION,
                world_name: "scripted".to_string(),
                seed: 42,
                spawn: [10.5, 70.0, 10.5],
                content_fingerprint: ob_protocol::content_fingerprint(&registry),
                tick: 1,
                day_ticks: ob_protocol::DAY_LENGTH_TICKS,
                max_players: 8,
                motd: "scripted".to_string(),
                blocks: ob_protocol::block_manifest(&registry),
            },
            ob_protocol::ServerMessage::Join {
                id: local.id,
                name: local.name.clone(),
            },
            ob_protocol::ServerMessage::PlayerState {
                id: local.id,
                name: local.name.clone(),
                pos: local.pos,
                yaw: local.yaw,
                pitch: local.pitch,
            },
            ob_protocol::ServerMessage::PlayerList {
                players: vec![local.clone()],
            },
        ];
        for message in &handshake {
            ob_protocol::write_message(&mut socket, message).expect("write handshake");
        }
        socket.flush().expect("flush handshake");
        control_tx
            .send(socket.try_clone().expect("clone control socket"))
            .expect("send control socket");
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("scripted server read timeout");
        let mut scratch = [0u8; 512];
        while matches!(socket.read(&mut scratch), Ok(read) if read > 0) {}
    });
    (addr, control_rx, thread)
}

fn send_server(control: &mut TcpStream, message: &ob_protocol::ServerMessage) {
    ob_protocol::write_message(control, message).expect("write scripted server message");
    control.flush().expect("flush scripted server message");
}

#[test]
fn hostile_server_payloads_are_bounded() {
    let (addr, control_rx, server) = start_scripted_server();
    let registry = Arc::new(Registry::base());
    let mut client = NetworkClient::connect(ConnectOptions::new(
        addr.to_string(),
        "tester",
        2,
        Arc::clone(&registry),
    ))
    .expect("connect scripted server");
    let mut control = control_rx.recv_timeout(WAIT).expect("scripted control socket");
    // Handshake messages are consumed by `connect`; send one post-join echo so
    // the last-known-good fallback has a value to compare against.
    send_server(
        &mut control,
        &ob_protocol::ServerMessage::PlayerState {
            id: 7,
            name: "tester".to_string(),
            pos: [10.0, 70.0, 10.0],
            yaw: 0.0,
            pitch: 0.0,
        },
    );
    wait_for(&mut client, "initial player state", |client| {
        client.last_server_state().is_some()
    });
    let initial = client
        .last_server_state()
        .map(|snapshot| snapshot.pos)
        .expect("initial state");

    // Huge finite respawn coordinates are clamped, not applied.
    send_server(
        &mut control,
        &ob_protocol::ServerMessage::Respawn {
            pos: [f64::MAX, -f64::MAX, f64::MAX],
            health: 20.0,
            hunger: 20.0,
        },
    );
    let events = wait_for(&mut client, "clamped respawn", |client| {
        client.player().pos.x >= 30_000_000.0
    });
    let position = client.player().pos;
    assert!(position.is_finite());
    assert!(position.x.abs() <= 30_000_000.0);
    assert!(position.z.abs() <= 30_000_000.0);
    assert!((-96.0..=576.0).contains(&position.y));
    assert!(
        events.iter().any(|event| matches!(
            event,
            NetEvent::Respawned { pos, .. } if pos.iter().all(|value| value.is_finite())
        )),
        "respawn event: {events:?}"
    );
    assert_eq!(
        client.last_server_state().map(|snapshot| snapshot.pos),
        Some(initial),
        "the last good server position is unchanged"
    );

    // An oversized roster is truncated at the explicit cap.
    let players: Vec<ob_protocol::PlayerSnapshot> = (0..(MAX_PLAYER_LIST + 32))
        .map(|index| ob_protocol::PlayerSnapshot {
            id: 1_000 + index as u64,
            name: format!("player{index}"),
            pos: [0.5, 70.0, 0.5],
            yaw: 0.0,
            pitch: 0.0,
        })
        .collect();
    send_server(&mut control, &ob_protocol::ServerMessage::PlayerList { players });
    wait_for(&mut client, "capped roster", |client| {
        client.player_count() == MAX_PLAYER_LIST + 1
    });
    assert_eq!(client.player_count(), MAX_PLAYER_LIST + 1);

    // An oversized entity snapshot keeps only the batch cap.
    let entities: Vec<ob_protocol::EntitySnapshot> = (0..(MAX_ENTITY_BATCH + 64))
        .map(|index| ob_protocol::EntitySnapshot {
            id: 10_000 + index as u64,
            kind: ob_protocol::EntityKind::Mob {
                mob: "openblock:cow".to_string(),
            },
            pos: [0.5, 70.0, 0.5],
            yaw: 0.0,
            health: 10.0,
            hurt: false,
        })
        .collect();
    send_server(&mut control, &ob_protocol::ServerMessage::EntitySnapshot { entities });
    wait_for(&mut client, "capped entity snapshot", |client| {
        client.entities().len() == MAX_ENTITY_BATCH
    });
    assert_eq!(client.entities().len(), MAX_ENTITY_BATCH);

    // An oversized state batch is applied only up to the same cap.
    let states: Vec<ob_protocol::EntityStateEntry> = (0..(MAX_ENTITY_BATCH + 64))
        .map(|index| ob_protocol::EntityStateEntry {
            id: 10_000 + index as u64,
            pos: [1.0, 70.0, 1.0],
            yaw: 0.0,
            health: 5.0,
            hurt: false,
            stack: None,
        })
        .collect();
    send_server(
        &mut control,
        &ob_protocol::ServerMessage::EntityState { entities: states },
    );
    wait_for(&mut client, "capped entity state", |client| {
        client.entities().health(10_000) == Some(5.0)
    });
    assert!(client.entities().len() <= MAX_ENTITY_BATCH);

    // Oversized chat fields are truncated and newlines cannot reach the UI.
    send_server(
        &mut control,
        &ob_protocol::ServerMessage::Chat {
            from: "s".repeat(1_000),
            text: "line\nbreak\r\tend".repeat(1_000),
        },
    );
    wait_for(&mut client, "capped chat", |client| !client.chat_log().is_empty());
    let line = client.chat_log().lines().last().expect("chat line");
    assert_eq!(line.from.chars().count(), MAX_CHAT_SENDER_CHARS);
    assert_eq!(line.text.chars().count(), MAX_CHAT_TEXT_CHARS);
    assert!(!line.from.chars().any(char::is_control));
    assert!(!line.text.chars().any(char::is_control));

    client.shutdown();
    drop(control);
    let _ = server.join();
}

/// One-off documentation probe: renders a survival frame from an in-process
/// server with the real network client. Ignored by default because it needs a
/// Vulkan adapter and writes a repository screenshot.
#[test]
#[ignore = "offscreen GPU demo; run manually to refresh screenshots/client-net-survival.png"]
fn network_survival_screenshot_probe() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("screenshots/client-net-survival.png");
    let debug_path = path.with_file_name("client-net-survival-debug.png");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("screenshot directory");
    }
    ob_client::screenshot_network_demo(&path, Some(&debug_path), 42).expect("network survival screenshot");
}

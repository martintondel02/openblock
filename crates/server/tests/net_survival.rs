//! Loopback integration tests for protocol v5 server-authoritative survival.
//!
//! Each test starts a real dedicated server on `127.0.0.1:0` with a temporary
//! world, connects raw `std::net::TcpStream` clients and exchanges the public
//! `ob_protocol` frames. No window, no external network.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_content::{BlockId, ItemId, MobKind, Registry};
use ob_math::{Aabb, BlockPos, LocalPos, WorldVec};
use ob_protocol::{
    ClientMessage, ContainerKind, DespawnReason, EntityKind, ItemStackDto, LIGHT_ENCODING_RAW, LIGHT_ENCODING_ZSTD,
    LIGHT_SECTION_FORMAT, MAX_FRAME_LEN, MAX_LIGHT_SECTIONS_PER_UPDATE, PROTOCOL_VERSION, PrecipitationKind,
    ServerMessage, WeatherPhase, decode_light_words, read_message, write_message,
};
use ob_server::{DedicatedServer, GameMode, Host, HostConfig, NetConfig};

const RECV_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT: Duration = Duration::from_secs(8);

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
            "openblock-net-survival-{label}-{}-{nanos}-{unique}",
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

struct TestServer {
    handle: Option<DedicatedServer>,
    #[allow(dead_code)]
    dir: TempDir,
    host_config: HostConfig,
    registry: Arc<Registry>,
    spawn: WorldVec,
}

impl TestServer {
    fn start(label: &str) -> Self {
        Self::start_with_mode(label, GameMode::Survival)
    }

    fn start_with_mode(label: &str, game_mode: GameMode) -> Self {
        Self::start_with_interval(label, game_mode, 3_600)
    }

    /// Starts at the beginning of the night so daylight burn cannot interfere
    /// with slow, charged-attack combat tests.
    fn start_at_night(label: &str) -> Self {
        Self::start_with_interval_at(label, GameMode::Survival, 3_600, 13_000)
    }

    fn start_with_interval(label: &str, game_mode: GameMode, save_interval_seconds: u64) -> Self {
        Self::start_with_interval_at(label, game_mode, save_interval_seconds, 0)
    }

    /// Starts at a fixed world tick (used by the sleep tests to begin at night).
    fn start_with_interval_at(label: &str, game_mode: GameMode, save_interval_seconds: u64, start_tick: u64) -> Self {
        let dir = TempDir::new(label);
        let registry = Arc::new(Registry::base());
        let host_config = HostConfig {
            seed: 42,
            world_name: "net-survival-test".to_string(),
            data_dir: Some(dir.path().to_path_buf()),
            render_distance_chunks: 2,
            generate_budget: 8,
        };
        let mut host = Host::open(Arc::clone(&registry), host_config.clone()).expect("open test world");
        if start_tick != 0 {
            host.set_tick_count(start_tick);
        }
        let spawn = host.spawn();
        let net_config = NetConfig {
            listen: "127.0.0.1:0".parse::<SocketAddr>().expect("ephemeral loopback"),
            max_players: 4,
            motd: "survival integration test".to_string(),
            interest_radius_chunks: 2,
            chunks_per_tick: 16,
            save_interval_seconds,
            whitelist: Vec::new(),
            game_mode,
        };
        let server = DedicatedServer::start(host, net_config).expect("start test server");
        Self {
            handle: Some(server),
            dir,
            host_config,
            registry,
            spawn,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.handle.as_ref().expect("running server").local_addr()
    }

    fn server(&self) -> &DedicatedServer {
        self.handle.as_ref().expect("running server")
    }

    fn stop(&mut self) {
        if let Some(server) = self.handle.take() {
            server.stop().expect("stop saves the world");
        }
    }

    fn world_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("worlds").join(&self.host_config.world_name)
    }

    fn reopen(&self) -> Host {
        let mut host = Host::open(Arc::clone(&self.registry), self.host_config.clone()).expect("reopen saved world");
        for _ in 0..100 {
            if host.ensure_streaming(32) == 0 {
                break;
            }
        }
        host
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(server) = self.handle.take() {
            let _ = server.stop();
        }
    }
}

struct TestClient {
    stream: TcpStream,
}

impl TestClient {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect to loopback server");
        stream.set_nodelay(true).expect("nodelay");
        stream.set_read_timeout(Some(RECV_TIMEOUT)).expect("read timeout");
        stream.set_write_timeout(Some(RECV_TIMEOUT)).expect("write timeout");
        Self { stream }
    }

    fn send(&mut self, message: ClientMessage) {
        write_message(&mut self.stream, &message).expect("write client frame");
        self.stream.flush().expect("flush client frame");
    }

    fn recv(&mut self) -> ServerMessage {
        read_message(&mut self.stream, MAX_FRAME_LEN).expect("read server frame")
    }

    fn recv_until(&mut self, what: &str, predicate: impl FnMut(&ServerMessage) -> bool) -> ServerMessage {
        self.recv_until_in(what, WAIT, predicate)
    }

    fn recv_until_in(
        &mut self,
        what: &str,
        timeout: Duration,
        mut predicate: impl FnMut(&ServerMessage) -> bool,
    ) -> ServerMessage {
        let deadline = Instant::now() + timeout;
        loop {
            let message = self.recv();
            if predicate(&message) {
                return message;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
        }
    }

    /// Sends a creative block edit and reports whether it was accepted.
    fn try_place_block(&mut self, pos: [i32; 3], block: u16) -> bool {
        self.send(ClientMessage::BlockEdit { pos, block });
        let deadline = Instant::now() + WAIT;
        loop {
            match self.recv() {
                ServerMessage::BlockEdit {
                    pos: edited,
                    block: placed,
                    ..
                } if edited == pos && placed == block => {
                    return true;
                }
                ServerMessage::ActionRejected { action, reason } if action == "block_edit" => {
                    let _ = reason;
                    return false;
                }
                _ => {}
            }
            assert!(Instant::now() < deadline, "timed out waiting for block edit at {pos:?}");
        }
    }

    /// Sends a creative block edit and panics if the server rejects it.
    fn place_block(&mut self, pos: [i32; 3], block: u16) {
        assert!(self.try_place_block(pos, block), "block edit at {pos:?} was rejected");
    }

    /// Walks a network player from `start` to `target` with server-legal steps.
    fn move_to(&mut self, start: WorldVec, target: WorldVec) {
        let deadline = Instant::now() + WAIT;
        let mut current = start;
        while current.distance(target) > 0.25 {
            assert!(
                Instant::now() < deadline,
                "could not move the test player to {target:?}"
            );
            let dx = (target.x - current.x).clamp(-1.9, 1.9);
            let dy = (target.y - current.y).clamp(-4.5, 4.5);
            let dz = (target.z - current.z).clamp(-1.9, 1.9);
            current = WorldVec::new(current.x + dx, current.y + dy, current.z + dz);
            self.send(ClientMessage::Move {
                pos: [current.x, current.y, current.z],
                yaw: 0.0,
                pitch: 0.0,
            });
            std::thread::sleep(Duration::from_millis(60));
        }
    }

    /// Waits for an `InventorySync` that contains `item` in the hotbar.
    fn wait_hotbar_slot(&mut self, item: &str) -> u8 {
        let deadline = Instant::now() + WAIT;
        loop {
            if let ServerMessage::InventorySync { hotbar, .. } = self.recv()
                && let Some(slot) = slot_of(&hotbar, item)
            {
                return slot;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {item} in the hotbar");
        }
    }

    /// Waits for the next `BlockEdit` at `pos` and returns its block id.
    fn wait_block_edit(&mut self, pos: [i32; 3]) -> u16 {
        let deadline = Instant::now() + WAIT;
        loop {
            if let ServerMessage::BlockEdit { pos: edited, block, .. } = self.recv()
                && edited == pos
            {
                return block;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a block edit at {pos:?}"
            );
        }
    }

    /// Waits for a `LightUpdate` and returns its sections.
    fn next_light_update(&mut self) -> Vec<ob_protocol::LightSectionDto> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let ServerMessage::LightUpdate { sections, .. } = self.recv() {
                return sections;
            }
            assert!(Instant::now() < deadline, "timed out waiting for LightUpdate");
        }
    }

    /// Waits for a `SleepState` matching `predicate`.
    fn sleep_state_until(&mut self, what: &str, predicate: impl FnMut(&ServerMessage) -> bool) -> ServerMessage {
        self.recv_until_in(what, WAIT, predicate)
    }

    fn expect_disconnect(&mut self, contains: &str) -> String {
        match self.recv_until("disconnect", |message| {
            matches!(message, ServerMessage::Disconnect { .. })
        }) {
            ServerMessage::Disconnect { reason } => {
                assert!(
                    reason.contains(contains),
                    "reason {reason:?} does not contain {contains:?}"
                );
                reason
            }
            _ => unreachable!(),
        }
    }
}

/// Handshake result including the initial authoritative sync frames.
struct Joined {
    id: u64,
    inventory: ServerMessage,
    vitals: ServerMessage,
    entity_snapshot: usize,
}

fn join(server: &TestServer, client: &mut TestClient, name: &str) -> Joined {
    client.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: name.to_string(),
    });
    client.recv_until("server hello", |message| matches!(message, ServerMessage::Hello { .. }));
    let join = client.recv_until(
        "own join",
        |message| matches!(message, ServerMessage::Join { name: joined, .. } if joined.as_str() == name),
    );
    let ServerMessage::Join { id, .. } = join else {
        unreachable!()
    };
    client.recv_until("player list", |message| {
        matches!(message, ServerMessage::PlayerList { .. })
    });
    let mut inventory = None;
    let mut vitals = None;
    let entity_snapshot;
    let deadline = Instant::now() + WAIT;
    loop {
        let message = client.recv();
        match message {
            ServerMessage::InventorySync { .. } => inventory = Some(message),
            ServerMessage::PlayerVitals { .. } => vitals = Some(message),
            ServerMessage::EntitySnapshot { entities } => {
                entity_snapshot = entities.len();
                break;
            }
            _ => {}
        }
        assert!(Instant::now() < deadline, "timed out waiting for initial survival sync");
    }
    let _ = server;
    Joined {
        id,
        inventory: inventory.expect("initial inventory sync"),
        vitals: vitals.expect("initial vitals"),
        entity_snapshot,
    }
}

fn inventory_of(message: &ServerMessage) -> ([Option<ItemStackDto>; 9], u8, [Option<ItemStackDto>; 4]) {
    match message {
        ServerMessage::InventorySync {
            hotbar,
            main: _,
            selected,
            equipment,
        } => (hotbar.clone(), *selected, equipment.clone()),
        _ => panic!("not an inventory sync"),
    }
}

fn count_in_hotbar(hotbar: &[Option<ItemStackDto>; 9], item: &str) -> u32 {
    hotbar
        .iter()
        .flatten()
        .filter(|stack| stack.item == item)
        .map(|stack| u32::from(stack.count))
        .sum()
}

fn slot_of(hotbar: &[Option<ItemStackDto>; 9], item: &str) -> Option<u8> {
    hotbar
        .iter()
        .position(|stack| stack.as_ref().is_some_and(|stack| stack.item == item))
        .map(|slot| slot as u8)
}

fn wait_player_position(server: &TestServer, id: u64, want: WorldVec) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(player) = server.server().server_player(id)
            && player.pos.distance(want) < 0.05
        {
            return;
        }
        assert!(Instant::now() < deadline, "server never accepted the expected position");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Finds an air cell with a solid neighbour that is safe to place into.
fn find_place_target(server: &TestServer, player: WorldVec) -> BlockPos {
    let base = BlockPos::new(
        player.x.floor() as i32,
        player.y.floor() as i32,
        player.z.floor() as i32,
    );
    for dy in 0..=2 {
        for dz in -3..=3 {
            for dx in -3..=3 {
                let pos = base.offset(dx, dy, dz);
                if server.server().block_at(pos) != Some(BlockId::AIR) {
                    continue;
                }
                let eye = WorldVec::new(player.x, player.y + 1.62, player.z);
                if pos.center().distance(eye) > 2.5 {
                    continue;
                }
                if Aabb::block(pos).intersects(&Aabb::new(
                    WorldVec::new(player.x - 0.3, player.y, player.z - 0.3),
                    WorldVec::new(player.x + 0.3, player.y + 1.8, player.z + 0.3),
                )) {
                    continue;
                }
                let supported = [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)]
                    .into_iter()
                    .any(|(lx, ly, lz)| {
                        server
                            .server()
                            .block_at(pos.offset(lx, ly, lz))
                            .is_some_and(|block| block != BlockId::AIR)
                    });
                if supported {
                    return pos;
                }
            }
        }
    }
    panic!("no placement target near {player:?}");
}

fn wait_slot(server: &TestServer, id: u64, item: ItemId) -> u8 {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(player) = server.server().server_player(id)
            && let Some(slot) = player
                .inventory
                .iter()
                .position(|stack| stack.is_some_and(|stack| stack.item == item))
        {
            return slot as u8;
        }
        assert!(Instant::now() < deadline, "item never entered the inventory");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn handshake_delivers_survival_state_and_entity_snapshot() {
    let mut server = TestServer::start("handshake");
    let mut alice = TestClient::connect(server.addr());
    let joined = join(&server, &mut alice, "alice");
    let (hotbar, selected, equipment) = inventory_of(&joined.inventory);
    assert!(hotbar.iter().all(Option::is_none));
    assert_eq!(selected, 0);
    assert!(equipment.iter().all(Option::is_none));
    match joined.vitals {
        ServerMessage::PlayerVitals {
            health,
            hunger,
            armor_defense,
        } => {
            assert_eq!(health, 20.0);
            assert_eq!(hunger, 20.0);
            assert_eq!(armor_defense, 0);
        }
        _ => unreachable!(),
    }
    assert!(joined.entity_snapshot < 64, "initial entity snapshot is bounded");
    assert_eq!(server.server().game_mode(), GameMode::Survival);
    server.stop();
}

#[test]
fn movement_bounds_and_speed_limits_are_enforced() {
    let mut server = TestServer::start("movement");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    let start = server.spawn;

    // A modest move is accepted and reflected in the authoritative state.
    let next = WorldVec::new(start.x + 1.0, start.y, start.z);
    alice.send(ClientMessage::Move {
        pos: [next.x, next.y, next.z],
        yaw: 0.0,
        pitch: 0.0,
    });
    wait_player_position(&server, id, next);

    // A huge teleport is dropped: the server keeps the accepted position.
    alice.send(ClientMessage::Move {
        pos: [next.x + 10_000.0, next.y + 10_000.0, next.z],
        yaw: 0.0,
        pitch: 0.0,
    });
    std::thread::sleep(Duration::from_millis(100));
    let player = server.server().server_player(id).expect("online");
    assert!(player.pos.distance(next) < 0.05, "teleport must be rejected");

    // Out-of-bounds values are clamped rather than panicking.
    alice.send(ClientMessage::Move {
        pos: [1e30, -1e30, 1e30],
        yaw: 0.0,
        pitch: 0.0,
    });
    std::thread::sleep(Duration::from_millis(100));
    let player = server.server().server_player(id).expect("online");
    assert!(player.pos.is_finite());
    server.stop();
}

#[test]
fn survival_place_break_drop_pickup_and_reach_rejection() {
    let mut server = TestServer::start("survival-actions");
    let mut alice = TestClient::connect(server.addr());
    let joined = join(&server, &mut alice, "alice");
    let id = joined.id;
    let stone_block = server.registry.id("openblock:stone").expect("stone block");
    assert!(server.server().give_network_item(id, "openblock:stone", 4));

    // Wait for the inventory sync carrying the stone and select it.
    let sync = alice.recv_until("stone inventory", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if count_in_hotbar(hotbar, "openblock:stone") == 4)
    });
    let (hotbar, _, _) = inventory_of(&sync);
    let slot = slot_of(&hotbar, "openblock:stone").expect("stone slot");
    alice.send(ClientMessage::SelectSlot { slot });
    alice.recv_until(
        "selected ack",
        |message| matches!(message, ServerMessage::SelectedSlot { slot: acked } if *acked == slot),
    );

    let spawn = server.server().server_player(id).expect("online").pos;
    let target = find_place_target(&server, spawn);
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: stone_block.0,
    });
    alice.recv_until("place broadcast", |message| {
        matches!(message, ServerMessage::BlockEdit { pos, block, player } if *pos == [target.x, target.y, target.z] && *block == stone_block.0 && *player == id)
    });
    let deadline = Instant::now() + WAIT;
    while server.server().block_at(target) != Some(stone_block) {
        assert!(Instant::now() < deadline, "placed block never became authoritative");
        std::thread::sleep(Duration::from_millis(10));
    }
    let sync = alice.recv_until("placement consumption", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if count_in_hotbar(hotbar, "openblock:stone") == 3)
    });
    assert_eq!(count_in_hotbar(&inventory_of(&sync).0, "openblock:stone"), 3);

    // Break the placed block; its loot spawns as an item entity and is picked
    // up by the same player, restoring the count. A pickaxe is required for the
    // stone drop table.
    assert!(server.server().give_network_item(id, "openblock:wooden_pickaxe", 1));
    let pickaxe = wait_slot(
        &server,
        id,
        server.registry.item_id("openblock:wooden_pickaxe").expect("pickaxe"),
    );
    alice.send(ClientMessage::SelectSlot { slot: pickaxe });
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: 0,
    });
    alice.recv_until("break broadcast", |message| {
        matches!(message, ServerMessage::BlockEdit { pos, block, player } if *pos == [target.x, target.y, target.z] && *block == 0 && *player == id)
    });
    let spawn_message = alice.recv_until("item entity spawn", |message| match message {
        ServerMessage::EntitySpawn {
            kind: EntityKind::Item { stack },
            ..
        } => stack.item == "openblock:cobblestone" && stack.count == 1,
        _ => false,
    });
    let ServerMessage::EntitySpawn { id: entity_id, .. } = spawn_message else {
        unreachable!()
    };
    alice.recv_until("item picked up despawn", |message| {
        matches!(message, ServerMessage::EntityDespawn { ids, reason: DespawnReason::PickedUp } if ids.contains(&entity_id))
    });
    alice.recv_until("inventory restored", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if count_in_hotbar(hotbar, "openblock:cobblestone") == 1)
    });

    // Out-of-reach edits and invalid slots are rejected with a reason.
    let far = target.offset(100, 0, 0);
    alice.send(ClientMessage::BlockEdit {
        pos: [far.x, far.y, far.z],
        block: 0,
    });
    alice.recv_until("out of reach", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "block_edit" && reason == "out_of_reach")
    });
    alice.send(ClientMessage::SelectSlot { slot: 42 });
    alice.recv_until("invalid slot", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "select_slot" && reason == "invalid_slot")
    });
    server.stop();
}

#[test]
fn starvation_damages_a_network_player() {
    let mut server = TestServer::start("starvation");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    assert!(server.server().set_network_player_vitals(id, 20.0, 0.0));
    let damage = alice.recv_until_in(
        "starvation damage",
        Duration::from_secs(8),
        |message| matches!(message, ServerMessage::DamageEvent { cause, .. } if cause == "starvation"),
    );
    let ServerMessage::DamageEvent { amount, .. } = damage else {
        unreachable!()
    };
    assert!(amount > 0.0);
    let player = server.server().server_player(id).expect("online");
    assert!(player.health < 20.0);
    assert!(!player.dead);
    server.stop();
}

#[test]
fn zombie_targets_and_damages_a_network_player() {
    let mut server = TestServer::start("zombie-target");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    let player = server.server().server_player(id).expect("online");
    let health = player.health;
    let mob = server
        .server()
        .spawn_mob(MobKind::Zombie, [player.pos.x + 1.0, player.pos.y, player.pos.z])
        .expect("spawn zombie");
    alice.recv_until(
        "zombie spawn",
        |message| matches!(message, ServerMessage::EntitySpawn { id, .. } if *id == mob),
    );
    let damage = alice.recv_until_in(
        "mob damage",
        Duration::from_secs(8),
        |message| matches!(message, ServerMessage::DamageEvent { cause, .. } if cause == "mob"),
    );
    let ServerMessage::DamageEvent { amount, .. } = damage else {
        unreachable!()
    };
    assert!(amount > 0.0);
    let player = server.server().server_player(id).expect("online");
    assert!(player.health < health);
    server.stop();
}

#[test]
fn fall_damage_death_and_respawn_flow() {
    let mut server = TestServer::start("fall-death");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    assert!(server.server().set_network_player_vitals(id, 20.0, 20.0));
    let start = server.server().server_player(id).expect("online").pos;

    // Report a controlled descent in one-metre steps, then a level "landing"
    // frame. Movement is client-authoritative, so this is the documented
    // fall-damage contract.
    for step in 1..=10 {
        alice.send(ClientMessage::Move {
            pos: [start.x, start.y - step as f64, start.z],
            yaw: 0.0,
            pitch: 0.0,
        });
        std::thread::sleep(Duration::from_millis(8));
    }
    alice.send(ClientMessage::Move {
        pos: [start.x, start.y - 10.0, start.z],
        yaw: 0.0,
        pitch: 0.0,
    });
    let damage = alice.recv_until(
        "fall damage",
        |message| matches!(message, ServerMessage::DamageEvent { cause, .. } if cause == "fall"),
    );
    let ServerMessage::DamageEvent { amount, .. } = damage else {
        unreachable!()
    };
    assert!(amount > 1.0, "a ten-metre fall must hurt ({amount})");
    alice.recv_until(
        "vitals after fall",
        |message| matches!(message, ServerMessage::PlayerVitals { health, .. } if *health < 20.0),
    );

    // Admin damage kills; death stops actions until Respawn.
    assert!(server.server().damage_network_player(id, 100.0));
    alice.recv_until("death", |message| matches!(message, ServerMessage::Death { .. }));
    alice.send(ClientMessage::SelectSlot { slot: 1 });
    alice.recv_until("action after death", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "select_slot" && reason == "dead")
    });
    alice.send(ClientMessage::Respawn);
    alice.recv_until("respawn", |message| {
        matches!(message, ServerMessage::Respawn { health, hunger, .. } if *health == 20.0 && *hunger == 20.0)
    });
    let player = server.server().server_player(id).expect("online");
    assert!(!player.dead);
    assert!(player.pos.distance(server.spawn) < 0.05);
    server.stop();
}

#[test]
fn eat_and_equip_armor_mitigate_damage() {
    let mut server = TestServer::start("eat-armor");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    assert!(server.server().set_network_player_vitals(id, 20.0, 10.0));
    assert!(server.server().give_network_item(id, "openblock:apple", 2));
    let sync = alice.recv_until("apple inventory", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if count_in_hotbar(hotbar, "openblock:apple") == 2)
    });
    let slot = slot_of(&inventory_of(&sync).0, "openblock:apple").expect("apple slot");
    alice.send(ClientMessage::SelectSlot { slot });
    alice.send(ClientMessage::Eat);
    alice.recv_until(
        "hunger restored",
        |message| matches!(message, ServerMessage::PlayerVitals { hunger, .. } if *hunger > 10.0),
    );
    alice.recv_until("apple consumed", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if count_in_hotbar(hotbar, "openblock:apple") == 1)
    });

    assert!(server.server().give_network_item(id, "openblock:iron_helmet", 1));
    assert!(server.server().give_network_item(id, "openblock:iron_chestplate", 1));
    let helmet_slot = wait_slot(
        &server,
        id,
        server.registry.item_id("openblock:iron_helmet").expect("helmet"),
    );
    alice.send(ClientMessage::Equip {
        hotbar_slot: helmet_slot,
    });
    let armored = alice.recv_until(
        "armor equipped",
        |message| matches!(message, ServerMessage::PlayerVitals { armor_defense, .. } if *armor_defense > 0),
    );
    let ServerMessage::PlayerVitals { armor_defense, .. } = armored else {
        unreachable!()
    };
    assert!(armor_defense >= 2);

    assert!(server.server().set_network_player_vitals(id, 20.0, 20.0));
    let health_before = server.server().server_player(id).expect("online").health;
    assert!(server.server().damage_network_player(id, 10.0));
    let damage = alice.recv_until(
        "mitigated damage",
        |message| matches!(message, ServerMessage::DamageEvent { amount, .. } if *amount < 10.0 && *amount > 0.0),
    );
    let ServerMessage::DamageEvent { amount, .. } = damage else {
        unreachable!()
    };
    assert!(amount < 10.0);
    let player = server.server().server_player(id).expect("online");
    assert!((player.health - (health_before - amount)).abs() < 0.01);
    assert!(player.equipment[0].is_some(), "helmet stays equipped");

    // Unequip via an explicit equipment-slot inventory move.
    let free = server
        .server()
        .server_player(id)
        .expect("online")
        .inventory
        .iter()
        .position(Option::is_none)
        .expect("free slot") as u16;
    alice.send(ClientMessage::InventoryMove {
        from: 36,
        to: free,
        count: 1,
        revision: None,
    });
    alice.recv_until(
        "armor unequipped",
        |message| matches!(message, ServerMessage::PlayerVitals { armor_defense, .. } if *armor_defense < 2),
    );
    let player = server.server().server_player(id).expect("online");
    assert!(player.equipment[0].is_none(), "helmet was unequipped");
    server.stop();
}

#[test]
fn attack_mob_with_tool_damage_durability_and_drops() {
    let mut server = TestServer::start_at_night("attack");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    assert!(server.server().give_network_item(id, "openblock:wooden_sword", 1));
    let slot = wait_slot(
        &server,
        id,
        server.registry.item_id("openblock:wooden_sword").expect("sword"),
    );
    alice.send(ClientMessage::SelectSlot { slot });
    let player = server.server().server_player(id).expect("online");
    let pos = player.pos;
    let mob = server
        .server()
        .spawn_mob(MobKind::Zombie, [pos.x + 1.5, pos.y, pos.z])
        .expect("spawn zombie");
    alice.recv_until(
        "zombie spawn",
        |message| matches!(message, ServerMessage::EntitySpawn { id, .. } if *id == mob),
    );
    let mut hits = 0;
    let mut killed = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while !killed && Instant::now() < deadline {
        // The server rejects a swing below 0.9 charge. Wait one full second so
        // the wall-clock attack cooldown (625 ms for a sword) is satisfied.
        std::thread::sleep(Duration::from_millis(950));
        alice.send(ClientMessage::Attack { target: Some(mob) });
        let result = alice.recv_until("attack result", |message| {
            matches!(message, ServerMessage::AttackResult { target, .. } if *target == mob)
                || matches!(message, ServerMessage::ActionRejected { action, .. } if action == "attack")
        });
        let ServerMessage::AttackResult {
            hit,
            damage,
            killed: dead,
            ..
        } = result
        else {
            panic!("attack rejected: {result:?}");
        };
        assert!(hit, "the zombie must be in reach");
        assert!(damage > 0.0);
        hits += 1;
        killed = dead;
    }
    assert!(killed, "the sword must eventually kill the zombie");
    assert!(hits >= 2, "a sword should need several hits ({hits})");
    alice.recv_until("killed despawn", |message| {
        matches!(message, ServerMessage::EntityDespawn { ids, reason: DespawnReason::Killed } if ids.contains(&mob))
    });
    let sync = alice.recv_until("tool durability", |message| match message {
        ServerMessage::InventorySync { hotbar, .. } => hotbar
            .iter()
            .flatten()
            .any(|stack| stack.item == "openblock:wooden_sword" && stack.durability.is_some_and(|d| d < 59)),
        _ => false,
    });
    let sword = inventory_of(&sync)
        .0
        .iter()
        .flatten()
        .find(|stack| stack.item == "openblock:wooden_sword")
        .cloned()
        .expect("sword");
    assert!(sword.durability.unwrap_or(0) < 59);
    server.stop();
}

#[test]
fn furnace_window_smelts_and_output_can_be_taken() {
    let mut server = TestServer::start("furnace");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    assert!(server.server().give_network_item(id, "openblock:furnace", 1));
    assert!(server.server().give_network_item(id, "openblock:coal", 1));
    assert!(server.server().give_network_item(id, "openblock:raw_iron", 1));
    let furnace_slot = wait_slot(
        &server,
        id,
        server.registry.item_id("openblock:furnace").expect("furnace"),
    );
    let coal_slot = wait_slot(&server, id, server.registry.item_id("openblock:coal").expect("coal"));
    let iron_slot = wait_slot(
        &server,
        id,
        server.registry.item_id("openblock:raw_iron").expect("raw iron"),
    );
    let furnace_block = server.registry.id("openblock:furnace").expect("furnace block");
    alice.send(ClientMessage::SelectSlot { slot: furnace_slot });
    let player = server.server().server_player(id).expect("online");
    let target = find_place_target(&server, player.pos);
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: furnace_block.0,
    });
    alice.recv_until("furnace placed", |message| {
        matches!(message, ServerMessage::BlockEdit { pos, block, .. } if *pos == [target.x, target.y, target.z] && *block == furnace_block.0)
    });
    alice.send(ClientMessage::UseBlock {
        pos: [target.x, target.y, target.z],
    });
    let opened = alice.recv_until("furnace open", |message| {
        matches!(
            message,
            ServerMessage::ContainerOpen {
                kind: ContainerKind::Furnace,
                ..
            }
        )
    });
    let ServerMessage::ContainerOpen { revision, .. } = opened else {
        unreachable!()
    };
    // A deliberately stale revision is rejected; functional moves below omit
    // the optional revision so a progress-driven revision bump cannot race the
    // test. The client contract is to echo the latest revision.
    alice.send(ClientMessage::InventoryMove {
        from: u16::from(coal_slot),
        to: 101,
        count: 1,
        revision: Some(revision + 100),
    });
    alice.recv_until("stale revision", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && reason == "stale_revision")
    });
    alice.send(ClientMessage::InventoryMove {
        from: u16::from(iron_slot),
        to: 100,
        count: 1,
        revision: None,
    });
    alice.recv_until("iron in input", |message| match message {
        ServerMessage::ContainerUpdate { slots, .. } => slots
            .first()
            .and_then(Option::as_ref)
            .is_some_and(|stack| stack.item == "openblock:raw_iron"),
        _ => false,
    });
    alice.send(ClientMessage::InventoryMove {
        from: u16::from(coal_slot),
        to: 101,
        count: 1,
        revision: None,
    });
    alice.recv_until("coal in fuel", |message| match message {
        ServerMessage::ContainerUpdate { slots, .. } => slots
            .get(1)
            .and_then(Option::as_ref)
            .is_some_and(|stack| stack.item == "openblock:coal"),
        _ => false,
    });
    // 200 ticks = 10 s of smelting. ContainerUpdate arrives at 5 Hz while open.
    let update = alice.recv_until_in("furnace output", Duration::from_secs(20), |message| match message {
        ServerMessage::ContainerUpdate { slots, furnace, .. } => {
            furnace.is_some_and(|furnace| furnace.burn_remaining > 0 || furnace.cook_progress > 0)
                && slots
                    .get(2)
                    .and_then(Option::as_ref)
                    .is_some_and(|stack| stack.item == "openblock:iron_ingot")
        }
        _ => false,
    });
    let ServerMessage::ContainerUpdate {
        revision: latest_revision,
        ..
    } = update
    else {
        unreachable!()
    };
    assert!(latest_revision > revision);
    let free_slot = server
        .server()
        .server_player(id)
        .expect("online")
        .inventory
        .iter()
        .position(Option::is_none)
        .expect("free slot") as u16;
    alice.send(ClientMessage::InventoryMove {
        from: 102,
        to: free_slot,
        count: 1,
        revision: None,
    });
    alice.recv_until("smelted ingot", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if count_in_hotbar(hotbar, "openblock:iron_ingot") == 1)
    });
    alice.send(ClientMessage::ContainerClose);
    alice.recv_until(
        "window closed",
        |message| matches!(message, ServerMessage::ContainerClose { reason, .. } if reason == "closed"),
    );
    server.stop();
}

#[test]
fn chest_window_transactions_persist_after_restart() {
    let mut server = TestServer::start("chest");
    let mut alice = TestClient::connect(server.addr());
    let mut bob = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    let _bob = join(&server, &mut bob, "bob").id;
    assert!(server.server().give_network_item(id, "openblock:chest", 1));
    assert!(server.server().give_network_item(id, "openblock:coal", 3));
    let chest_slot = wait_slot(&server, id, server.registry.item_id("openblock:chest").expect("chest"));
    let coal_slot = wait_slot(&server, id, server.registry.item_id("openblock:coal").expect("coal"));
    let chest_block = server.registry.id("openblock:chest").expect("chest block");
    alice.send(ClientMessage::SelectSlot { slot: chest_slot });
    let player = server.server().server_player(id).expect("online");
    let target = find_place_target(&server, player.pos);
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: chest_block.0,
    });
    alice.recv_until("chest placed", |message| {
        matches!(message, ServerMessage::BlockEdit { pos, block, .. } if *pos == [target.x, target.y, target.z] && *block == chest_block.0)
    });
    alice.send(ClientMessage::UseBlock {
        pos: [target.x, target.y, target.z],
    });
    let opened = alice.recv_until("chest open", |message| {
        matches!(
            message,
            ServerMessage::ContainerOpen {
                kind: ContainerKind::Chest,
                ..
            }
        )
    });
    let ServerMessage::ContainerOpen {
        window,
        slots,
        revision,
        ..
    } = opened
    else {
        unreachable!()
    };
    assert_eq!(window, 1);
    assert_eq!(slots.len(), 27);
    assert_eq!(revision, 1);

    // A second player has no window of its own, so window addresses are
    // rejected even if it guesses Alice's window id.
    bob.send(ClientMessage::InventoryMove {
        from: u16::from(coal_slot),
        to: 100,
        count: 1,
        revision: Some(1),
    });
    bob.recv_until("bob has no window", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && reason == "no_window")
    });

    alice.send(ClientMessage::InventoryMove {
        from: u16::from(coal_slot),
        to: 100,
        count: 2,
        revision: None,
    });
    alice.recv_until("coal deposited", |message| match message {
        ServerMessage::ContainerUpdate { slots, .. } => slots
            .first()
            .and_then(Option::as_ref)
            .is_some_and(|stack| stack.item == "openblock:coal" && stack.count == 2),
        _ => false,
    });
    alice.recv_until("coal inventory reduced", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if count_in_hotbar(hotbar, "openblock:coal") == 1)
    });
    alice.send(ClientMessage::InventoryMove {
        from: 100,
        to: u16::from(coal_slot),
        count: 1,
        revision: None,
    });
    alice.recv_until("coal withdrawn", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if count_in_hotbar(hotbar, "openblock:coal") == 2)
    });
    alice.send(ClientMessage::ContainerClose);
    alice.recv_until(
        "chest closed",
        |message| matches!(message, ServerMessage::ContainerClose { reason, .. } if reason == "closed"),
    );

    // Re-open and verify the authoritative slots.
    alice.send(ClientMessage::UseBlock {
        pos: [target.x, target.y, target.z],
    });
    alice.recv_until("chest reopened", |message| match message {
        ServerMessage::ContainerOpen {
            kind: ContainerKind::Chest,
            slots,
            ..
        } => slots
            .first()
            .and_then(Option::as_ref)
            .is_some_and(|stack| stack.item == "openblock:coal" && stack.count == 1),
        _ => false,
    });
    alice.send(ClientMessage::ContainerClose);
    alice.recv_until("chest closed again", |message| {
        matches!(message, ServerMessage::ContainerClose { .. })
    });

    alice.send(ClientMessage::Disconnect);
    drop(alice);
    drop(bob);
    server.stop();
    let host = server.reopen();
    let chest = host.chest_at(target).expect("persisted chest");
    assert_eq!(
        chest.slots[0].expect("coal").count,
        1,
        "chest contents must survive a save/reload"
    );
}

#[test]
fn network_breeding_emits_interact_results_xp_and_a_baby_entity() {
    let mut server = TestServer::start_at_night("net-breed");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    assert!(server.server().give_network_item(id, "openblock:wheat", 8));
    let slot = wait_slot(&server, id, server.registry.item_id("openblock:wheat").expect("wheat"));
    alice.send(ClientMessage::SelectSlot { slot });
    let player = server.server().server_player(id).expect("online");
    let first = server
        .server()
        .spawn_mob(MobKind::Cow, [player.pos.x + 1.2, player.pos.y, player.pos.z])
        .expect("cow one");
    let second = server
        .server()
        .spawn_mob(MobKind::Cow, [player.pos.x + 2.0, player.pos.y, player.pos.z])
        .expect("cow two");
    alice.send(ClientMessage::InteractEntity { id: first });
    alice.recv_until("first feed result", |message| {
        matches!(message, ServerMessage::InteractResult { id: target, accepted: true, .. } if *target == first)
    });
    alice.send(ClientMessage::InteractEntity { id: second });
    alice.recv_until("pair feed result", |message| {
        matches!(message, ServerMessage::InteractResult { id: target, accepted: true, .. } if *target == second)
    });
    alice.recv_until(
        "xp sync after breeding",
        |message| matches!(message, ServerMessage::XpSync { points, .. } if *points > 0),
    );
    // The 60-tick gestation is visible as a baby mob spawn carrying `baby`.
    alice.recv_until_in("baby entity spawn", Duration::from_secs(6), |message| {
        matches!(
            message,
            ServerMessage::EntitySpawn {
                kind: EntityKind::Mob { mob },
                baby: true,
                ..
            } if mob == "openblock:cow"
        )
    });
    server.stop();
}

#[test]
fn network_skeleton_arrow_is_replicated_as_a_projectile_entity() {
    let mut server = TestServer::start_at_night("net-arrow");
    let mut alice = TestClient::connect(server.addr());
    let _ = join(&server, &mut alice, "alice");
    let player = server
        .server()
        .server_players()
        .into_iter()
        .find(|p| p.name == "alice")
        .expect("alice");
    let skeleton = server
        .server()
        .spawn_mob(MobKind::Skeleton, [player.pos.x + 10.0, player.pos.y, player.pos.z])
        .expect("skeleton");
    alice.recv_until(
        "skeleton spawn",
        |message| matches!(message, ServerMessage::EntitySpawn { id, .. } if *id == skeleton),
    );
    alice.recv_until_in("arrow spawn", Duration::from_secs(4), |message| {
        matches!(
            message,
            ServerMessage::EntitySpawn {
                kind: EntityKind::Projectile { projectile },
                ..
            } if projectile == "openblock:arrow"
        )
    });
    server.stop();
}

#[test]
fn mob_drops_spawn_as_items_and_are_picked_up() {
    let mut server = TestServer::start_at_night("mob-drops");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    assert!(server.server().give_network_item(id, "openblock:wooden_sword", 1));
    let slot = wait_slot(
        &server,
        id,
        server.registry.item_id("openblock:wooden_sword").expect("sword"),
    );
    alice.send(ClientMessage::SelectSlot { slot });
    let player = server.server().server_player(id).expect("online");
    let cow = server
        .server()
        .spawn_mob(MobKind::Cow, [player.pos.x + 1.2, player.pos.y, player.pos.z])
        .expect("spawn cow");
    alice.recv_until(
        "cow spawn",
        |message| matches!(message, ServerMessage::EntitySpawn { id, .. } if *id == cow),
    );
    let mut killed = false;
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(950));
        alice.send(ClientMessage::Attack { target: Some(cow) });
        let result = alice.recv_until(
            "cow attack",
            |message| matches!(message, ServerMessage::AttackResult { target, .. } if *target == cow),
        );
        if let ServerMessage::AttackResult { killed: dead, hit, .. } = result {
            assert!(hit, "the cow should still be in reach");
            killed = dead;
        }
        if killed {
            break;
        }
    }
    assert!(killed, "the sword must kill the cow");
    let drop = alice.recv_until("cow drop", |message| match message {
        ServerMessage::EntitySpawn {
            kind: EntityKind::Item { stack },
            ..
        } => stack.item == "openblock:raw_beef" || stack.item == "openblock:leather",
        _ => false,
    });
    let ServerMessage::EntitySpawn { id: entity_id, pos, .. } = drop else {
        unreachable!()
    };
    // Charged combat takes long enough for the fleeing cow to die a few metres
    // away; walk the test client onto the drop so pickup can run.
    alice.send(ClientMessage::Move {
        pos,
        yaw: 0.0,
        pitch: 0.0,
    });
    alice.recv_until("mob drop pickup", |message| {
        matches!(message, ServerMessage::EntityDespawn { ids, reason: DespawnReason::PickedUp } if ids.contains(&entity_id))
    });
    alice.recv_until("mob loot in inventory", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if hotbar.iter().flatten().any(|stack| stack.item == "openblock:raw_beef" || stack.item == "openblock:leather"))
    });
    server.stop();
}

#[test]
fn death_closes_an_open_container_window() {
    let mut server = TestServer::start("death-window");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    assert!(server.server().give_network_item(id, "openblock:chest", 1));
    let chest_slot = wait_slot(&server, id, server.registry.item_id("openblock:chest").expect("chest"));
    let chest_block = server.registry.id("openblock:chest").expect("chest block");
    alice.send(ClientMessage::SelectSlot { slot: chest_slot });
    let player = server.server().server_player(id).expect("online");
    let target = find_place_target(&server, player.pos);
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: chest_block.0,
    });
    alice.recv_until("chest placed", |message| {
        matches!(message, ServerMessage::BlockEdit { pos, block, .. } if *pos == [target.x, target.y, target.z] && *block == chest_block.0)
    });
    alice.send(ClientMessage::UseBlock {
        pos: [target.x, target.y, target.z],
    });
    let window = alice.recv_until("chest open", |message| {
        matches!(
            message,
            ServerMessage::ContainerOpen {
                kind: ContainerKind::Chest,
                ..
            }
        )
    });
    let ServerMessage::ContainerOpen { window, .. } = window else {
        unreachable!()
    };
    assert!(server.server().damage_network_player(id, 100.0));
    // ContainerClose is queued before Death during the same relay, so wait for
    // the close first instead of discarding it while looking for Death.
    alice.recv_until("window closed on death", |message| {
        matches!(message, ServerMessage::ContainerClose { window: closed, reason } if *closed == window && reason == "death")
    });
    alice.recv_until("death", |message| matches!(message, ServerMessage::Death { .. }));
    server.stop();
}

#[test]
fn second_client_sees_entity_spawn_state_and_despawn() {
    let mut server = TestServer::start("replication");
    let mut alice = TestClient::connect(server.addr());
    let mut bob = TestClient::connect(server.addr());
    let alice_id = join(&server, &mut alice, "alice").id;
    let _bob_id = join(&server, &mut bob, "bob").id;
    assert!(server.server().give_network_item(alice_id, "openblock:stone", 2));
    assert!(
        server
            .server()
            .give_network_item(alice_id, "openblock:wooden_pickaxe", 1)
    );

    let player = server.server().server_player(alice_id).expect("online");
    let target = find_place_target(&server, player.pos);
    let stone = server.registry.id("openblock:stone").expect("stone block");
    let sync = alice.recv_until("stone sync", |message| {
        matches!(message, ServerMessage::InventorySync { hotbar, .. } if count_in_hotbar(hotbar, "openblock:stone") == 2)
    });
    let slot = slot_of(&inventory_of(&sync).0, "openblock:stone").expect("stone slot");
    alice.send(ClientMessage::SelectSlot { slot });
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: stone.0,
    });
    bob.recv_until(
        "bob block edit",
        |message| matches!(message, ServerMessage::BlockEdit { pos, .. } if *pos == [target.x, target.y, target.z]),
    );
    let pickaxe = wait_slot(
        &server,
        alice_id,
        server.registry.item_id("openblock:wooden_pickaxe").expect("pickaxe"),
    );
    alice.send(ClientMessage::SelectSlot { slot: pickaxe });
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: 0,
    });
    let spawn = bob.recv_until("bob item spawn", |message| match message {
        ServerMessage::EntitySpawn {
            kind: EntityKind::Item { stack },
            ..
        } => stack.item == "openblock:cobblestone",
        _ => false,
    });
    let ServerMessage::EntitySpawn { id: entity_id, .. } = spawn else {
        unreachable!()
    };
    // Alice is standing near the block and picks the drop up; Bob sees the
    // authoritative despawn. The pickup inventory sync arrives after the
    // despawn, so do not consume and discard one waiting for the other.
    bob.recv_until("bob item picked up", |message| {
        matches!(message, ServerMessage::EntityDespawn { ids, reason: DespawnReason::PickedUp } if ids.contains(&entity_id))
    });
    // Bob also receives Alice's player state from the 10 Hz broadcast.
    bob.recv_until(
        "bob sees alice state",
        |message| matches!(message, ServerMessage::PlayerState { id, .. } if *id == alice_id),
    );

    // A mob spawned after Bob joined is replicated and its movement arrives as
    // a batched EntityState entry.
    let alice_pos = server.server().server_player(alice_id).expect("online").pos;
    let cow = server
        .server()
        .spawn_mob(MobKind::Cow, [alice_pos.x + 2.0, alice_pos.y, alice_pos.z])
        .expect("spawn cow");
    bob.recv_until(
        "bob sees cow spawn",
        |message| matches!(message, ServerMessage::EntitySpawn { id, .. } if *id == cow),
    );
    bob.recv_until("bob sees cow state", |message| {
        matches!(message, ServerMessage::EntityState { entities } if entities.iter().any(|state| state.id == cow))
    });
    server.stop();
}

#[test]
fn idle_connected_player_is_autosaved_on_the_configured_interval() {
    // Regression: the tick advances persisted state (tick count, hunger/regen,
    // player position after Move, mobs/furnaces/items) but only explicit intents
    // used to set the net layer's dirty flag, so an idle connection was never
    // autosaved and a crash lost everything since the previous action.
    let mut server = TestServer::start_with_interval("autosave", GameMode::Survival, 1);
    let mut alice = TestClient::connect(server.addr());
    let _id = join(&server, &mut alice, "autosave-alice").id;
    let path = server.world_dir().join("network_players.txt");
    let deadline = Instant::now() + WAIT;
    loop {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if text.contains("autosave-alice") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the connected player was never autosaved; network_players.txt = {text:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    server.stop();
    let saved = server
        .reopen()
        .saved_network_player("autosave-alice")
        .cloned()
        .expect("the autosaved record loads");
    assert!(saved.pos.is_finite());
}

#[test]
fn reconnect_and_restart_restore_network_player_state() {
    let mut server = TestServer::start("persist");
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    assert!(server.server().give_network_item(id, "openblock:apple", 3));
    assert!(server.server().set_network_player_vitals(id, 14.0, 9.0));
    alice.send(ClientMessage::SelectSlot { slot: 0 });
    let moved = WorldVec::new(server.spawn.x + 2.0, server.spawn.y, server.spawn.z);
    alice.send(ClientMessage::Move {
        pos: [moved.x, moved.y, moved.z],
        yaw: 0.5,
        pitch: -0.25,
    });
    wait_player_position(&server, id, moved);
    alice.send(ClientMessage::Disconnect);
    drop(alice);
    let deadline = Instant::now() + WAIT;
    while server.server().player_count() != 0 {
        assert!(Instant::now() < deadline, "player was not removed");
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut reconnect = TestClient::connect(server.addr());
    let joined = join(&server, &mut reconnect, "alice");
    let (hotbar, _, _) = inventory_of(&joined.inventory);
    assert_eq!(count_in_hotbar(&hotbar, "openblock:apple"), 3);
    match joined.vitals {
        ServerMessage::PlayerVitals { health, hunger, .. } => {
            assert_eq!(health, 14.0);
            assert!((hunger - 9.0).abs() < 0.2, "hunger {hunger} should restore near 9.0");
        }
        _ => unreachable!(),
    }
    let restored = server.server().server_player(joined.id).expect("online");
    assert!(restored.pos.distance(moved) < 0.05);
    assert!((restored.yaw - 0.5).abs() < 0.01);
    reconnect.send(ClientMessage::Disconnect);
    drop(reconnect);
    server.stop();

    let host = server.reopen();
    let saved = host.saved_network_player("alice").expect("saved network player");
    assert_eq!(saved.health, 14.0);
    assert!((saved.hunger - 9.0).abs() < 0.2, "saved hunger {}", saved.hunger);
    assert!(saved.pos.distance(moved) < 0.05);
    assert_eq!(
        saved
            .inventory
            .iter()
            .flatten()
            .filter(|stack| stack.item == host.registry().item_id("openblock:apple").unwrap())
            .map(|stack| stack.count)
            .sum::<u8>(),
        3
    );
}

#[test]
fn thirty_six_slot_inventory_moves_equips_and_survives_reconnect() {
    let mut server = TestServer::start("inventory36");
    let mut alice = TestClient::connect(server.addr());
    let joined = join(&server, &mut alice, "alice");
    assert!(server.server().give_network_item(joined.id, "openblock:stone", 40));
    assert!(server.server().give_network_item(joined.id, "openblock:iron_helmet", 1));

    // Initial sync: all items start in the nine hotbar slots and the 27 main
    // slots are explicitly present and empty.
    let sync = alice.recv_until("initial hotbar items", |message| match message {
        ServerMessage::InventorySync { hotbar, main, .. } => {
            main.iter().all(Option::is_none)
                && hotbar.iter().flatten().any(|stack| stack.item == "openblock:stone")
                && hotbar
                    .iter()
                    .flatten()
                    .any(|stack| stack.item == "openblock:iron_helmet")
        }
        _ => false,
    });
    let ServerMessage::InventorySync { hotbar, main, .. } = sync else {
        unreachable!()
    };
    assert_eq!(main.len(), 27);
    assert_eq!(
        hotbar
            .iter()
            .flatten()
            .filter(|stack| stack.item == "openblock:stone")
            .map(|stack| u32::from(stack.count))
            .sum::<u32>(),
        40
    );
    let helmet_slot = hotbar
        .iter()
        .position(|slot| slot.as_ref().is_some_and(|stack| stack.item == "openblock:iron_helmet"))
        .expect("helmet in the hotbar");

    // Hotbar slot 0 -> main slot 9 (wire address 0 -> 9).
    alice.send(ClientMessage::InventoryMove {
        from: 0,
        to: 9,
        count: 40,
        revision: None,
    });
    alice.recv_until("stone moved to main", |message| match message {
        ServerMessage::InventorySync { hotbar, main, .. } => {
            hotbar
                .iter()
                .all(|slot| slot.as_ref().is_none_or(|stack| stack.item != "openblock:stone"))
                && main[0]
                    .as_ref()
                    .is_some_and(|stack| stack.item == "openblock:stone" && stack.count == 40)
        }
        _ => false,
    });

    // Main slot 9 -> hotbar slot 0 for a partial stack.
    alice.send(ClientMessage::InventoryMove {
        from: 9,
        to: 0,
        count: 20,
        revision: None,
    });
    alice.recv_until("half stone moved back", |message| match message {
        ServerMessage::InventorySync { hotbar, main, .. } => {
            hotbar[0]
                .as_ref()
                .is_some_and(|stack| stack.item == "openblock:stone" && stack.count == 20)
                && main[0]
                    .as_ref()
                    .is_some_and(|stack| stack.item == "openblock:stone" && stack.count == 20)
        }
        _ => false,
    });

    // Equip the helmet from its hotbar slot using the new equipment address 36.
    alice.send(ClientMessage::InventoryMove {
        from: helmet_slot as u16,
        to: 36,
        count: 1,
        revision: None,
    });
    alice.recv_until("helmet equipped at address 36", |message| match message {
        ServerMessage::InventorySync { hotbar, equipment, .. } => {
            equipment[0]
                .as_ref()
                .is_some_and(|stack| stack.item == "openblock:iron_helmet")
                && hotbar[helmet_slot].is_none()
        }
        _ => false,
    });

    // Reconnect with the same name: the inventory and equipment are restored.
    alice.send(ClientMessage::Disconnect);
    drop(alice);
    let deadline = Instant::now() + WAIT;
    while server.server().player_count() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut again = TestClient::connect(server.addr());
    let rejoined = join(&server, &mut again, "alice");
    let ServerMessage::InventorySync {
        hotbar,
        main,
        selected,
        equipment,
    } = rejoined.inventory
    else {
        unreachable!()
    };
    assert_eq!(selected, 0);
    assert_eq!(main.len(), 27);
    assert!(
        main[0]
            .as_ref()
            .is_some_and(|stack| stack.item == "openblock:stone" && stack.count == 20)
    );
    assert!(
        hotbar[0]
            .as_ref()
            .is_some_and(|stack| stack.item == "openblock:stone" && stack.count == 20)
    );
    assert!(
        equipment[0]
            .as_ref()
            .is_some_and(|stack| stack.item == "openblock:iron_helmet")
    );
    again.send(ClientMessage::Disconnect);
    drop(again);
    server.stop();
}

#[test]
fn protocol_v4_client_is_rejected_with_reason() {
    let mut server = TestServer::start("protocol");
    let mut old = TestClient::connect(server.addr());
    old.send(ClientMessage::Hello {
        protocol: 4,
        name: "legacy".to_string(),
    });
    let reason = old.expect_disconnect("protocol_mismatch");
    assert!(reason.contains("server protocol 5"), "{reason}");
    assert!(reason.contains("upgrade"), "{reason}");
    assert_eq!(ob_protocol::PROTOCOL_VERSION, 5);
    server.stop();
}

#[test]
fn creative_network_mode_keeps_instant_edits_and_immunity() {
    let mut server = TestServer::start_with_mode("creative", GameMode::Creative);
    let mut alice = TestClient::connect(server.addr());
    let id = join(&server, &mut alice, "alice").id;
    let target = find_place_target(&server, server.spawn);
    let stone = server.registry.id("openblock:stone").expect("stone block");
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: stone.0,
    });
    alice.recv_until(
        "creative edit",
        |message| matches!(message, ServerMessage::BlockEdit { pos, .. } if *pos == [target.x, target.y, target.z]),
    );
    assert!(server.server().damage_network_player(id, 100.0));
    std::thread::sleep(Duration::from_millis(100));
    let player = server.server().server_player(id).expect("online");
    assert!(!player.dead);
    assert_eq!(player.health, 20.0);
    server.stop();
}
/// Decodes a `LightUpdate` section and returns the packed word at `pos`.
fn light_word_in_sections(sections: &[ob_protocol::LightSectionDto], pos: BlockPos) -> Option<u16> {
    let section = pos.section();
    let dto = sections
        .iter()
        .find(|dto| dto.cx == section.x && dto.cy == section.y && dto.cz == section.z)?;
    assert_eq!(dto.format, LIGHT_SECTION_FORMAT);
    let words = decode_light_words(dto.encoding, &dto.data).expect("valid light words");
    assert_eq!(words.len(), ob_protocol::LIGHT_CELLS);
    let local = LocalPos {
        x: (pos.x - section.x * 16) as u8,
        y: (pos.y - section.y * 16) as u8,
        z: (pos.z - section.z * 16) as u8,
    };
    Some(words[local.index()])
}

#[test]
fn light_update_round_trips_dense_sections_over_loopback() {
    let server = TestServer::start_with_mode("light-update", GameMode::Creative);
    let mut alice = TestClient::connect(server.addr());
    let joined = join(&server, &mut alice, "alice");
    let spawn = server.spawn;
    let sx = spawn.x.floor() as i32;
    let sz = spawn.z.floor() as i32;
    let stone = server.registry.id("openblock:stone").expect("stone").0;
    let torch = server.registry.id("openblock:torch").expect("torch").0;
    let build = WorldVec::new(sx as f64 + 0.5, 100.0, sz as f64 + 0.5);
    alice.move_to(spawn, build);
    wait_player_position(&server, joined.id, build);
    let base = [sx, 100, sz];
    alice.place_block(base, stone);
    alice.place_block([sx, 101, sz], torch);
    let sections = alice.next_light_update();
    assert!(!sections.is_empty());
    assert!(sections.len() <= MAX_LIGHT_SECTIONS_PER_UPDATE);
    for dto in &sections {
        assert_eq!(dto.format, LIGHT_SECTION_FORMAT);
        assert!(dto.encoding == LIGHT_ENCODING_RAW || dto.encoding == LIGHT_ENCODING_ZSTD);
    }
    let pos = BlockPos::new(sx, 101, sz);
    let word = light_word_in_sections(&sections, pos).expect("torch section is in the batch");
    assert_eq!((word >> 8) & 0xF, 14, "red block light at the torch");
    assert_eq!((word >> 4) & 0xF, 14, "green block light at the torch");
    assert_eq!(word & 0xF, 14, "blue block light at the torch");
}

#[test]
fn multiplayer_sleep_requires_every_player_and_fast_forwards_to_sunrise() {
    let mut server = TestServer::start_with_interval_at("sleep-loopback", GameMode::Creative, 3_600, 15_000);
    let mut alice = TestClient::connect(server.addr());
    let alice_joined = join(&server, &mut alice, "alice");
    let mut bob = TestClient::connect(server.addr());
    let bob_joined = join(&server, &mut bob, "bob");

    let spawn = server.spawn;
    let sx = spawn.x.floor() as i32;
    let sz = spawn.z.floor() as i32;
    let stone = server.registry.id("openblock:stone").expect("stone").0;
    let bed_foot = server.registry.id("openblock:bed_foot").expect("bed_foot").0;
    // Both players move next to the future bed (client-authoritative movement),
    // then a floating platform high above the terrain keeps the bed cells clear
    // of trees and slopes.
    let near_foot = WorldVec::new(sx as f64 + 0.5, 102.0, sz as f64 + 0.5);
    let near_head = WorldVec::new(sx as f64 + 0.5, 102.0, sz as f64 + 1.5);
    alice.move_to(spawn, near_foot);
    bob.move_to(spawn, near_head);
    wait_player_position(&server, alice_joined.id, near_foot);
    wait_player_position(&server, bob_joined.id, near_head);
    alice.place_block([sx, 100, sz], stone);
    alice.place_block([sx, 100, sz + 1], stone);
    alice.place_block([sx, 101, sz], bed_foot);

    alice.send(ClientMessage::SleepRequest { pos: [sx, 101, sz] });
    let state = alice.sleep_state_until("alice asleep", |message| {
        matches!(
            message,
            ServerMessage::SleepState {
                sleeping: true,
                all_sleeping: false,
                ..
            }
        )
    });
    let ServerMessage::SleepState { sleepers, bed, .. } = state else {
        unreachable!()
    };
    assert_eq!(sleepers, vec![alice_joined.id]);
    assert_eq!(bed, Some([sx, 101, sz]));

    bob.send(ClientMessage::SleepRequest { pos: [sx, 101, sz] });
    bob.sleep_state_until("everyone asleep", |message| {
        matches!(
            message,
            ServerMessage::SleepState {
                sleeping: true,
                all_sleeping: true,
                ..
            }
        )
    });
    let sunrise = bob.recv_until("sunrise TimeSync", |message| {
        matches!(message, ServerMessage::TimeSync { day_phase: 0, .. })
    });
    let ServerMessage::TimeSync { tick, .. } = sunrise else {
        unreachable!()
    };
    assert!(tick >= 24_000, "fast-forward reaches sunrise, got {tick}");
    bob.sleep_state_until("woken at sunrise", |message| {
        matches!(
            message,
            ServerMessage::SleepState {
                sleeping: false,
                all_sleeping: false,
                ..
            }
        )
    });

    // The bed respawn point persists in the network-player sidecar.
    server.stop();
    let text = std::fs::read_to_string(server.world_dir().join("network_players.txt")).expect("network players");
    assert!(text.contains(" r"), "respawn token written: {text}");
    let host = server.reopen();
    let alice_record = host.saved_network_player("alice").expect("alice record");
    assert_eq!(
        alice_record.respawn_point,
        Some(WorldVec::new(sx as f64 + 0.5, 102.0, sz as f64 + 0.5))
    );
    let bob_record = host.saved_network_player("bob").expect("bob record");
    assert!(bob_record.respawn_point.is_some());
}
#[test]
fn farming_use_block_updates_replicate_to_clients() {
    let server = TestServer::start_with_mode("farming-sync", GameMode::Creative);
    let mut alice = TestClient::connect(server.addr());
    let joined = join(&server, &mut alice, "alice");
    let spawn = server.spawn;
    let sx = spawn.x.floor() as i32;
    let sz = spawn.z.floor() as i32;
    let near = WorldVec::new(sx as f64 + 0.5, 200.0, sz as f64 + 0.5);
    alice.move_to(spawn, near);
    wait_player_position(&server, joined.id, near);

    let dirt = server.registry.id("openblock:dirt").expect("dirt").0;
    let stone = server.registry.id("openblock:stone").expect("stone").0;
    let torch = server.registry.id("openblock:torch").expect("torch").0;
    let farmland_dry = server.registry.id("openblock:farmland_dry").expect("farmland").0;
    let wheat_0 = server.registry.id("openblock:wheat_0").expect("wheat_0").0;

    let soil = [sx, 198, sz];
    alice.place_block(soil, dirt);
    // A lit torch beside the crop keeps the planting light rule satisfied.
    alice.place_block([sx + 1, 197, sz], stone);
    alice.place_block([sx + 1, 198, sz], torch);

    assert!(server.server().give_network_item(joined.id, "openblock:wood_hoe", 1));
    let hoe_slot = alice.wait_hotbar_slot("openblock:wood_hoe");
    alice.send(ClientMessage::SelectSlot { slot: hoe_slot });
    alice.recv_until(
        "hoe selected",
        |message| matches!(message, ServerMessage::SelectedSlot { slot } if *slot == hoe_slot),
    );
    alice.send(ClientMessage::UseBlock { pos: soil });
    assert_eq!(alice.wait_block_edit(soil), farmland_dry);

    assert!(server.server().give_network_item(joined.id, "openblock:seeds", 4));
    let seeds_slot = alice.wait_hotbar_slot("openblock:seeds");
    alice.send(ClientMessage::SelectSlot { slot: seeds_slot });
    alice.recv_until(
        "seeds selected",
        |message| matches!(message, ServerMessage::SelectedSlot { slot } if *slot == seeds_slot),
    );
    alice.send(ClientMessage::UseBlock { pos: soil });
    assert_eq!(alice.wait_block_edit([sx, 199, sz]), wheat_0);

    // Harvesting the immature crop removes the block and replicates the edit.
    alice.send(ClientMessage::UseBlock { pos: [sx, 199, sz] });
    assert_eq!(alice.wait_block_edit([sx, 199, sz]), 0);
}

#[test]
fn protocol_v5_weather_sync_lightning_fire_and_reconnect_are_authoritative() {
    let mut server = TestServer::start("weather-v5");
    let mut alice = TestClient::connect(server.addr());
    let joined = join(&server, &mut alice, "alice");

    let sync = alice.recv_until("weather sync", |message| {
        matches!(message, ServerMessage::WeatherSync { .. })
    });
    let ServerMessage::WeatherSync {
        phase,
        intensity,
        remaining_ticks,
        epoch,
        precipitation,
        biome,
        weather_factor,
    } = sync
    else {
        unreachable!()
    };
    let authoritative = server.server().weather_state();
    assert_eq!(phase, authoritative.phase);
    assert!(
        remaining_ticks.abs_diff(authoritative.remaining_ticks) <= 60,
        "the sync and the live state may be a few ticks apart"
    );
    assert_eq!(epoch, authoritative.epoch);
    assert!((intensity - f64::from(authoritative.intensity_permille) / 1_000.0).abs() < 1e-9);
    assert!(matches!(
        biome.as_str(),
        "plains" | "forest" | "desert" | "snowy_taiga" | "mountains"
    ));
    if phase == WeatherPhase::Clear {
        assert_eq!(precipitation, PrecipitationKind::None);
        assert_eq!(weather_factor, 1.0);
    }

    // Force a full thunderstorm, freeze the machine and verify the next sync.
    server.server().set_weather_state(WeatherPhase::Thunder, 10_000, 1_000);
    server.server().set_weather_cycle(false);
    let sync = alice.recv_until("thunder weather sync", |message| {
        matches!(
            message,
            ServerMessage::WeatherSync {
                phase: WeatherPhase::Thunder,
                ..
            }
        )
    });
    let ServerMessage::WeatherSync {
        intensity,
        remaining_ticks,
        epoch,
        precipitation,
        biome,
        weather_factor,
        ..
    } = sync
    else {
        unreachable!()
    };
    assert_eq!(intensity, 1.0);
    assert!((9_990..=10_000).contains(&remaining_ticks));
    assert_eq!(epoch, 0);
    assert_eq!(weather_factor, 0.6);
    assert!(
        precipitation != PrecipitationKind::None || biome == "desert",
        "a precipitating phase must rain/snow everywhere except a dry biome"
    );

    // Force one strike at the surface directly below the spawn feet. The
    // broadcast must carry the flash and the following fire block edit.
    let spawn = server.spawn;
    let top = BlockPos::new(
        spawn.x.floor() as i32,
        spawn.y.floor() as i32 - 1,
        spawn.z.floor() as i32,
    );
    // A stone roof directly above the future fire keeps it out of the storm, so
    // the fire BlockEdit is observed instead of being coalesced away by rain.
    // The admin surface writes the block without survival reach/support rules.
    assert!(
        server
            .server()
            .set_block(BlockPos::new(top.x, top.y + 2, top.z), "openblock:stone"),
        "roof block must be writable"
    );
    assert!(
        server.server().force_lightning_at(top),
        "spawn surface must accept a strike"
    );
    let strike = alice.recv_until("lightning strike", |message| {
        matches!(message, ServerMessage::LightningStrike { .. })
    });
    let ServerMessage::LightningStrike {
        pos,
        block,
        ignited,
        flash,
    } = strike
    else {
        unreachable!()
    };
    assert_eq!(block, [top.x, top.y, top.z]);
    assert!(flash);
    assert!(pos[1] > f64::from(top.y));
    assert!(ignited, "an air cell above a solid surface must ignite");
    let fire_id = Registry::base().id("openblock:fire").expect("fire block id").0;
    let fire_pos = [top.x, top.y + 1, top.z];
    let edited = alice.wait_block_edit(fire_pos);
    assert_eq!(edited, fire_id, "fire ignition must arrive as a BlockEdit");

    // Reconnect: the new session sees the same frozen storm and fire.
    drop(alice);
    let mut bob = TestClient::connect(server.addr());
    join(&server, &mut bob, "bob");
    let sync = bob.recv_until("weather sync after reconnect", |message| {
        matches!(message, ServerMessage::WeatherSync { .. })
    });
    let ServerMessage::WeatherSync { phase, epoch, .. } = sync else {
        unreachable!()
    };
    assert_eq!(phase, WeatherPhase::Thunder);
    assert_eq!(epoch, 0);

    let expected = server.server().weather_state();
    assert!(server.server().block_at(top.offset(0, 1, 0)).is_some());
    server.stop();
    let reopened = server.reopen();
    assert_eq!(reopened.weather(), &expected, "weather must survive save/reload");
    assert_eq!(
        reopened.registry().id_or_air("openblock:fire"),
        reopened
            .world()
            .get_block(BlockPos::new(fire_pos[0], fire_pos[1], fire_pos[2]))
    );
    assert!(reopened.tick_count() > 0);
    assert!(joined.id != 0);
}

//! Loopback integration tests for protocol v2 server-authoritative survival.
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
use ob_math::{Aabb, BlockPos, WorldVec};
use ob_protocol::{
    ClientMessage, ContainerKind, DespawnReason, EntityKind, ItemStackDto, MAX_FRAME_LEN, PROTOCOL_VERSION,
    ServerMessage, read_message, write_message,
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
        let dir = TempDir::new(label);
        let registry = Arc::new(Registry::base());
        let host_config = HostConfig {
            seed: 42,
            world_name: "net-survival-test".to_string(),
            data_dir: Some(dir.path().to_path_buf()),
            render_distance_chunks: 2,
            generate_budget: 8,
        };
        let host = Host::open(Arc::clone(&registry), host_config.clone()).expect("open test world");
        let spawn = host.spawn();
        let net_config = NetConfig {
            listen: "127.0.0.1:0".parse::<SocketAddr>().expect("ephemeral loopback"),
            max_players: 4,
            motd: "survival integration test".to_string(),
            interest_radius_chunks: 2,
            chunks_per_tick: 16,
            save_interval_seconds: 3_600,
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
        from: 9,
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
    let mut server = TestServer::start("attack");
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
        alice.send(ClientMessage::Attack { target: Some(mob) });
        let result = alice.recv_until(
            "attack result",
            |message| matches!(message, ServerMessage::AttackResult { target, .. } if *target == mob),
        );
        let ServerMessage::AttackResult {
            hit,
            damage,
            killed: dead,
            ..
        } = result
        else {
            unreachable!()
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
fn mob_drops_spawn_as_items_and_are_picked_up() {
    let mut server = TestServer::start("mob-drops");
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
    let ServerMessage::EntitySpawn { id: entity_id, .. } = drop else {
        unreachable!()
    };
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
fn protocol_v1_client_is_rejected_with_reason() {
    let mut server = TestServer::start("protocol");
    let mut old = TestClient::connect(server.addr());
    old.send(ClientMessage::Hello {
        protocol: 1,
        name: "legacy".to_string(),
    });
    let reason = old.expect_disconnect("protocol_mismatch");
    assert!(reason.contains("server protocol 2"), "{reason}");
    assert_eq!(ob_protocol::PROTOCOL_VERSION, 2);
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

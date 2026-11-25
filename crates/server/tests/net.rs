//! Loopback integration tests for the interim TCP dedicated server.
//!
//! Each test starts a real server on `127.0.0.1:0`, connects raw
//! `std::net::TcpStream` clients, and exchanges the public `ob_protocol`
//! frames. No external network, no window, no sleeping on wall-clock luck:
//! every wait is a deadline around a blocking frame read or a short poll.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_content::{BlockId, Registry};
use ob_math::{BlockPos, ChunkPos, WorldVec};
use ob_protocol::{
    BlockDescriptor, ClientMessage, MAX_FRAME_LEN, PROTOCOL_VERSION, PlayerSnapshot, ServerMessage,
    content_fingerprint, decode_chunk, encode_client_frame, encode_server_frame, manifest_matches, read_frame,
    write_message,
};
use ob_server::{DedicatedServer, GameMode, Host, HostConfig, NetConfig};
use ob_world::World;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT: Duration = Duration::from_secs(8);

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
            "openblock-net-it-{label}-{}-{nanos}-{unique}",
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
    dir: TempDir,
    host_config: HostConfig,
    registry: Arc<Registry>,
    spawn: WorldVec,
    seed: u64,
}

impl TestServer {
    fn start(label: &str, max_players: u16, whitelist: Vec<String>) -> Self {
        Self::start_with_mode(label, max_players, whitelist, GameMode::Survival)
    }

    fn start_creative(label: &str, max_players: u16, whitelist: Vec<String>) -> Self {
        Self::start_with_mode(label, max_players, whitelist, GameMode::Creative)
    }

    fn start_with_mode(label: &str, max_players: u16, whitelist: Vec<String>, game_mode: GameMode) -> Self {
        let dir = TempDir::new(label);
        let registry = Arc::new(Registry::base());
        let host_config = HostConfig {
            seed: 42,
            world_name: "net-test".to_string(),
            data_dir: Some(dir.path().to_path_buf()),
            render_distance_chunks: 2,
            generate_budget: 8,
        };
        let host = Host::open(Arc::clone(&registry), host_config.clone()).expect("open test world");
        let spawn = host.spawn();
        let seed = host.world().seed();
        let net_config = NetConfig {
            listen: "127.0.0.1:0".parse::<SocketAddr>().expect("ephemeral loopback"),
            max_players,
            motd: "integration test".to_string(),
            interest_radius_chunks: 2,
            chunks_per_tick: 16,
            save_interval_seconds: 3_600,
            whitelist,
            game_mode,
        };
        let server = DedicatedServer::start(host, net_config).expect("start test server");
        Self {
            handle: Some(server),
            dir,
            host_config,
            registry,
            spawn,
            seed,
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
        assert!(self.dir.path().is_dir(), "data directory must outlive the server");
        let config = self.host_config.clone();
        let mut host = Host::open(Arc::clone(&self.registry), config).expect("reopen saved world");
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
    next_ping: u32,
}

impl TestClient {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect to loopback server");
        stream.set_nodelay(true).expect("nodelay");
        stream.set_read_timeout(Some(RECV_TIMEOUT)).expect("read timeout");
        stream.set_write_timeout(Some(RECV_TIMEOUT)).expect("write timeout");
        Self { stream, next_ping: 1 }
    }

    fn send(&mut self, message: ClientMessage) {
        write_message(&mut self.stream, &message).expect("write client frame");
        self.stream.flush().expect("flush client frame");
    }

    fn recv(&mut self) -> ServerMessage {
        let message: ServerMessage =
            ob_protocol::read_message(&mut self.stream, MAX_FRAME_LEN).expect("read server frame");
        message
    }

    fn recv_until(&mut self, what: &str, mut predicate: impl FnMut(&ServerMessage) -> bool) -> ServerMessage {
        let deadline = Instant::now() + WAIT;
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
                    "disconnect reason {reason:?} does not contain {contains:?}"
                );
                reason
            }
            _ => unreachable!(),
        }
    }

    fn ping(&mut self) -> u32 {
        let id = self.next_ping;
        self.next_ping = self.next_ping.wrapping_add(1);
        self.send(ClientMessage::Ping { id });
        id
    }
}

fn join(server: &TestServer, client: &mut TestClient, name: &str) -> u64 {
    client.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: name.to_string(),
    });
    let hello = client.recv_until("server hello", |message| matches!(message, ServerMessage::Hello { .. }));
    let ServerMessage::Hello {
        protocol,
        content_fingerprint: fingerprint,
        blocks,
        ..
    } = hello
    else {
        unreachable!()
    };
    assert_eq!(protocol, PROTOCOL_VERSION);
    assert_eq!(fingerprint, content_fingerprint(&server.registry));
    assert!(manifest_matches(&blocks, &server.registry));
    assert!(blocks.iter().any(|block| block.name == "openblock:stone"));

    let join = client.recv_until(
        "own join",
        |message| matches!(message, ServerMessage::Join { name: joined, .. } if joined.as_str() == name),
    );
    let ServerMessage::Join { id, .. } = join else {
        unreachable!()
    };
    let roster = client.recv_until("player list", |message| {
        matches!(message, ServerMessage::PlayerList { .. })
    });
    let ServerMessage::PlayerList { players } = roster else {
        unreachable!()
    };
    assert!(players.iter().any(|player| player.id == id && player.name == name));
    id
}

fn join_pair(server: &TestServer) -> (TestClient, u64, TestClient, u64) {
    let mut alice = TestClient::connect(server.addr());
    let alice_id = join(server, &mut alice, "alice");
    let mut bob = TestClient::connect(server.addr());
    let bob_id = join(server, &mut bob, "bob");
    (alice, alice_id, bob, bob_id)
}

#[test]
fn handshake_streams_chunks_that_decode_and_insert() {
    let mut server = TestServer::start("handshake", 4, Vec::new());
    let mut client = TestClient::connect(server.addr());
    let id = join(&server, &mut client, "alice");
    assert!(id >= 1);

    // Automatic streaming must deliver at least one column promptly.
    let chunk_message = client.recv_until("streamed chunk", |message| {
        matches!(message, ServerMessage::ChunkData { .. })
    });
    let ServerMessage::ChunkData { cx, cz, bytes } = chunk_message else {
        unreachable!()
    };
    let column = decode_chunk(&server.registry, cx, cz, &bytes).expect("decode streamed chunk");
    assert_eq!(column.pos(), ChunkPos::new(cx, cz));

    // Decoded blocks agree with the authoritative world, and the column can be
    // inserted into a fresh client-side World.
    let mut world = World::new(Arc::clone(&server.registry), server.seed);
    world.insert_chunk(column.clone());
    let mut compared = 0;
    for z in 0..16 {
        for x in 0..16 {
            let block_pos = BlockPos::new(cx * 16 + x, 64, cz * 16 + z);
            if let Some(authoritative) = server.server().block_at(block_pos) {
                assert_eq!(world.get_block(block_pos), authoritative);
                compared += 1;
            }
        }
    }
    assert!(compared > 0, "at least one sampled block must be resident");

    // An explicit request for a nearby column is answered even if automatic
    // streaming had already sent it.
    let requested = ChunkPos::new(cx + 1, cz);
    client.send(ClientMessage::ChunkRequest {
        cx: requested.x,
        cz: requested.z,
    });
    let requested_message = client.recv_until("requested chunk", |message| {
        matches!(message, ServerMessage::ChunkData { cx, cz, .. } if *cx == requested.x && *cz == requested.z)
    });
    let ServerMessage::ChunkData { bytes, .. } = requested_message else {
        unreachable!()
    };
    let requested_column = decode_chunk(&server.registry, requested.x, requested.z, &bytes).expect("decode requested");
    assert_eq!(requested_column.pos(), requested);

    client.send(ClientMessage::Ping { id: 99 });
    let pong = client.recv_until("pong", |message| matches!(message, ServerMessage::Pong { id: 99 }));
    assert!(matches!(pong, ServerMessage::Pong { id: 99 }));
    server.stop();
}

#[test]
fn two_clients_exchange_movement_chat_and_authoritative_edits() {
    let mut server = TestServer::start_creative("gameplay", 4, Vec::new());
    let (mut alice, alice_id, mut bob, bob_id) = join_pair(&server);
    assert_ne!(alice_id, bob_id);

    // Alice sees Bob's Join and later state; Bob's roster already has Alice.
    let join = alice.recv_until(
        "bob join",
        |message| matches!(message, ServerMessage::Join { id, name } if *id == bob_id && name.as_str() == "bob"),
    );
    assert!(matches!(join, ServerMessage::Join { .. }));
    let state = alice.recv_until(
        "bob state",
        |message| matches!(message, ServerMessage::PlayerState { id, .. } if *id == bob_id),
    );
    assert!(matches!(state, ServerMessage::PlayerState { .. }));
    alice.send(ClientMessage::Ping { id: 1 });
    alice.recv_until("alice pong", |message| matches!(message, ServerMessage::Pong { id: 1 }));

    // Client-authoritative move relayed as a PlayerState to both clients.
    let moved = WorldVec::new(server.spawn.x + 1.0, server.spawn.y, server.spawn.z + 1.0);
    alice.send(ClientMessage::Move {
        pos: [moved.x, moved.y, moved.z],
        yaw: 0.25,
        pitch: -0.125,
    });
    let check_move = |message: &ServerMessage| match message {
        ServerMessage::PlayerState {
            id, pos, yaw, pitch, ..
        } if *id == alice_id => {
            (pos[0] - moved.x).abs() < 0.01
                && (pos[1] - moved.y).abs() < 0.01
                && (pos[2] - moved.z).abs() < 0.01
                && (*yaw - 0.25).abs() < 0.001
                && (*pitch + 0.125).abs() < 0.001
        }
        _ => false,
    };
    alice.recv_until("own moved state", check_move);
    bob.recv_until("alice moved state", check_move);

    // Chat is relayed with the sender's name.
    alice.send(ClientMessage::Chat {
        text: "  hello bob  ".to_string(),
    });
    let alice_chat = alice.recv_until("own chat", |message| matches!(message, ServerMessage::Chat { .. }));
    let bob_chat = bob.recv_until("relayed chat", |message| matches!(message, ServerMessage::Chat { .. }));
    for message in [alice_chat, bob_chat] {
        match message {
            ServerMessage::Chat { from, text } => {
                assert_eq!(from, "alice");
                assert_eq!(text, "hello bob");
            }
            _ => unreachable!(),
        }
    }

    // Place stone at a known air cell within reach. Wait until the server has
    // the column resident before editing.
    let stone = server.registry.id("openblock:stone").expect("stone id");
    let target = BlockPos::new(
        moved.x.floor() as i32,
        moved.y.floor() as i32 + 1,
        moved.z.floor() as i32,
    );
    let deadline = Instant::now() + WAIT;
    while server.server().block_at(target).is_none() {
        assert!(Instant::now() < deadline, "target chunk never became resident");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.server().block_at(target), Some(BlockId::AIR));
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: stone.0,
    });
    for (client, label) in [(&mut alice, "alice"), (&mut bob, "bob")] {
        let edit = client.recv_until("block edit broadcast", |message| {
            matches!(message, ServerMessage::BlockEdit { pos, block, player }
                if *pos == [target.x, target.y, target.z] && *block == stone.0 && *player == alice_id)
        });
        assert!(matches!(edit, ServerMessage::BlockEdit { .. }), "{label} got the edit");
    }
    assert_eq!(server.server().block_at(target), Some(stone));
    let (accepted, rejected) = server.server().edit_counts();
    assert_eq!(accepted, 1);
    assert_eq!(rejected, 0);

    // Out-of-reach edit is rejected and does not change the world.
    let far = BlockPos::new(target.x + 100, target.y, target.z);
    alice.send(ClientMessage::BlockEdit {
        pos: [far.x, far.y, far.z],
        block: stone.0,
    });
    alice.ping();
    alice.recv_until("pong after rejected edit", |message| {
        matches!(message, ServerMessage::Pong { .. })
    });
    assert_ne!(server.server().block_at(far), Some(stone));
    let (accepted, rejected) = server.server().edit_counts();
    assert_eq!(accepted, 1);
    assert_eq!(rejected, 1);

    // A voluntary disconnect broadcasts Leave and removes the player.
    alice.send(ClientMessage::Disconnect);
    let leave = bob.recv_until(
        "alice leave",
        |message| matches!(message, ServerMessage::Leave { id } if *id == alice_id),
    );
    assert!(matches!(leave, ServerMessage::Leave { .. }));
    let deadline = Instant::now() + WAIT;
    while server.server().player_count() != 1 {
        assert!(Instant::now() < deadline, "server did not remove alice");
        std::thread::sleep(Duration::from_millis(10));
    }
    bob.send(ClientMessage::Ping { id: 7 });
    bob.recv_until("server still alive", |message| {
        matches!(message, ServerMessage::Pong { id: 7 })
    });

    // Stop and reload: the authoritative edit must survive save/load.
    drop(alice);
    drop(bob);
    server.stop();
    let reopened = server.reopen();
    assert_eq!(reopened.world().get_block(target), stone);
}

#[test]
fn whitelist_max_players_and_protocol_mismatch_are_rejected() {
    let mut server = TestServer::start("admission", 1, vec!["alice".to_string()]);
    let mut alice = TestClient::connect(server.addr());
    join(&server, &mut alice, "alice");

    // Second connection is rejected because the server is full.
    let mut bob = TestClient::connect(server.addr());
    bob.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: "bob".to_string(),
    });
    bob.expect_disconnect("server_full");

    // A protocol mismatch is rejected with an actionable reason.
    let mut future = TestClient::connect(server.addr());
    future.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION + 1,
        name: "future".to_string(),
    });
    future.expect_disconnect("protocol_mismatch");

    // Invalid names are rejected.
    let mut invalid = TestClient::connect(server.addr());
    invalid.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: "bad name".to_string(),
    });
    invalid.expect_disconnect("invalid_name");

    // Oversized frame headers are rejected before allocation.
    let mut oversize = TcpStream::connect(server.addr()).expect("connect");
    oversize.set_read_timeout(Some(RECV_TIMEOUT)).expect("read timeout");
    let mut header = [0u8; 4];
    header.copy_from_slice(&(MAX_FRAME_LEN as u32 + 1).to_be_bytes());
    oversize.write_all(&header).expect("write oversized header");
    let mut reason = String::new();
    let message: ServerMessage = ob_protocol::read_message(&mut oversize, MAX_FRAME_LEN).expect("disconnect frame");
    if let ServerMessage::Disconnect { reason: text } = message {
        reason = text;
    }
    assert!(reason.contains("frame_too_large"), "{reason:?}");

    // A malformed JSON body is a protocol error, not a panic.
    let mut malformed = TestClient::connect(server.addr());
    let mut frame = vec![0u8, 0, 0, 1];
    frame.push(b'{');
    malformed.stream.write_all(&frame).expect("write malformed frame");
    malformed.expect_disconnect("protocol_error");
    server.stop();
}

#[test]
fn whitelist_rejects_unknown_names_and_allows_listed_ones() {
    let mut server = TestServer::start("whitelist", 4, vec!["alice".to_string()]);
    let mut bob = TestClient::connect(server.addr());
    bob.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: "bob".to_string(),
    });
    bob.expect_disconnect("not_whitelisted");
    let mut alice = TestClient::connect(server.addr());
    join(&server, &mut alice, "alice");
    server.stop();
}

#[test]
fn duplicate_names_and_duplicate_hello_are_rejected() {
    let mut server = TestServer::start("duplicates", 4, Vec::new());
    let mut alice = TestClient::connect(server.addr());
    join(&server, &mut alice, "alice");
    let mut duplicate = TestClient::connect(server.addr());
    duplicate.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: "Alice".to_string(),
    });
    duplicate.expect_disconnect("name_taken");

    alice.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: "alice".to_string(),
    });
    alice.expect_disconnect("duplicate hello");
    server.stop();
}

#[test]
fn console_kick_disconnects_the_named_player() {
    let mut server = TestServer::start("kick", 4, Vec::new());
    let (mut alice, alice_id, mut bob, _bob_id) = join_pair(&server);
    assert!(server.server().kick("alice", "kicked for testing"));
    alice.expect_disconnect("kicked for testing");
    let leave = bob.recv_until(
        "leave after kick",
        |message| matches!(message, ServerMessage::Leave { id } if *id == alice_id),
    );
    assert!(matches!(leave, ServerMessage::Leave { .. }));
    let deadline = Instant::now() + WAIT;
    while server.server().player_count() != 1 {
        assert!(Instant::now() < deadline, "kick did not remove the player");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!server.server().kick("nobody", "x"));
    server.stop();
}

#[test]
fn graceful_shutdown_disconnects_clients_and_saves() {
    let mut server = TestServer::start("shutdown", 4, Vec::new());
    let mut alice = TestClient::connect(server.addr());
    join(&server, &mut alice, "alice");
    server.server().request_shutdown();
    alice.expect_disconnect("server_shutdown");
    server.stop();
}

#[test]
fn frames_are_length_prefixed_and_encoding_is_stable() {
    let message = ClientMessage::Ping { id: 123 };
    let frame = encode_client_frame(&message).expect("encode");
    let payload_len = u32::from_be_bytes(frame[..4].try_into().expect("header")) as usize;
    assert_eq!(payload_len, frame.len() - 4);
    let mut reader = frame.as_slice();
    let payload = read_frame(&mut reader, MAX_FRAME_LEN).expect("read frame");
    assert_eq!(payload.len(), payload_len);
    let server_message = ServerMessage::Chat {
        from: "server".to_string(),
        text: "hello".to_string(),
    };
    let first = encode_server_frame(&server_message).expect("encode");
    let second = encode_server_frame(&server_message).expect("encode");
    assert_eq!(first, second);
}

#[test]
fn core_protocol_version_matches_ob_protocol() {
    assert_eq!(ob_core::PROTOCOL_VERSION, ob_protocol::PROTOCOL_VERSION);
    assert_eq!(ob_core::PROTOCOL_VERSION, 4);
    assert!(ob_core::version_banner("openblock-server").contains("protocol 4"));
}

/// Keeps the import list honest when the protocol grows: every public message
/// type used by these tests is exercised at least once above.
#[allow(dead_code)]
fn types_are_public(_: BlockDescriptor, _: PlayerSnapshot) {}

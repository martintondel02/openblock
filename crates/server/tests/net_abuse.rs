//! Adversarial loopback tests for the interim TCP protocol.
//!
//! Every test starts a real [`DedicatedServer`] on `127.0.0.1:0` with a
//! temporary world and talks raw TCP, so the tests exercise the same framing,
//! rate limits, validation and disconnect paths a hostile client would. No
//! window, no external network and no GPU.

use std::io::{ErrorKind, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_content::{MobKind, Registry};
use ob_math::{BlockPos, WorldVec};
use ob_protocol::{
    ClientMessage, MAX_FRAME_LEN, PROTOCOL_VERSION, ProtocolError, ServerMessage, read_message, write_message,
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
            "openblock-net-abuse-{label}-{}-{nanos}-{unique}",
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
    registry: Arc<Registry>,
    spawn: WorldVec,
}

impl TestServer {
    fn start(label: &str, max_players: u16) -> Self {
        Self::start_with_saved(label, max_players, None)
    }

    fn start_with_saved(label: &str, max_players: u16, saved_players: Option<&str>) -> Self {
        let dir = TempDir::new(label);
        let registry = Arc::new(Registry::base());
        let host_config = HostConfig {
            seed: 42,
            world_name: "net-abuse-test".to_string(),
            data_dir: Some(dir.path().to_path_buf()),
            render_distance_chunks: 2,
            generate_budget: 8,
        };
        if let Some(text) = saved_players {
            // The first open creates world.obs and overwrites a sidecar written
            // before it, so create the world first, then plant the sidecar.
            let _ = Host::open(Arc::clone(&registry), host_config.clone()).expect("create test world");
            let world_dir = dir.path().join("worlds").join("net-abuse-test");
            std::fs::write(world_dir.join("network_players.txt"), text).expect("write saved network players");
        }
        let host = Host::open(Arc::clone(&registry), host_config).expect("open test world");
        let spawn = host.spawn();
        let net_config = NetConfig {
            listen: "127.0.0.1:0".parse::<SocketAddr>().expect("ephemeral loopback"),
            max_players,
            motd: "adversarial test".to_string(),
            interest_radius_chunks: 2,
            chunks_per_tick: 4,
            save_interval_seconds: 0,
            whitelist: Vec::new(),
            game_mode: GameMode::Survival,
        };
        let server = DedicatedServer::start(host, net_config).expect("start test server");
        Self {
            handle: Some(server),
            dir,
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
        let _ = self.dir.path();
        if let Some(server) = self.handle.take() {
            server.stop().expect("stop saves the world");
        }
    }

    fn wait_player_count(&self, want: usize) {
        let deadline = Instant::now() + WAIT;
        while self.server().player_count() != want {
            assert!(Instant::now() < deadline, "server never reached {want} players");
            std::thread::sleep(Duration::from_millis(10));
        }
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

    fn send_payload(&mut self, payload: &[u8]) {
        self.stream
            .write_all(&(payload.len() as u32).to_be_bytes())
            .expect("write frame length");
        self.stream.write_all(payload).expect("write frame payload");
        self.stream.flush().expect("flush raw frame");
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).expect("write raw bytes");
        self.stream.flush().expect("flush raw bytes");
    }

    fn recv(&mut self) -> ServerMessage {
        read_message(&mut self.stream, MAX_FRAME_LEN).expect("read server frame")
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

    /// Reads whatever arrives until the deadline, treating a read timeout as the
    /// end of the batch and restoring the normal timeout afterwards.
    fn drain_for(&mut self, duration: Duration) -> Vec<ServerMessage> {
        self.stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("short timeout");
        let deadline = Instant::now() + duration;
        let mut messages = Vec::new();
        while Instant::now() < deadline {
            match read_message::<_, ServerMessage>(&mut self.stream, MAX_FRAME_LEN) {
                Ok(message) => messages.push(message),
                Err(ProtocolError::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    continue;
                }
                Err(_) => break,
            }
        }
        self.stream
            .set_read_timeout(Some(RECV_TIMEOUT))
            .expect("restore timeout");
        messages
    }

    fn shutdown_write(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Write);
    }
}

/// Handshake result: the connection id plus the number of entities in the
/// initial snapshot.
fn join(_server: &TestServer, client: &mut TestClient, name: &str) -> (u64, usize) {
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
    let deadline = Instant::now() + WAIT;
    let entity_snapshot = loop {
        if let ServerMessage::EntitySnapshot { entities } = client.recv() {
            break entities.len();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the initial entity snapshot"
        );
    };
    (id, entity_snapshot)
}

/// Connects, writes raw bytes and expects the server's rejection reason. The
/// caller uses this for malformed handshakes.
fn expect_handshake_rejection(server: &TestServer, bytes: &[u8], partial: bool, expected: &str) {
    let mut stream = TcpStream::connect(server.addr()).expect("connect");
    stream.set_read_timeout(Some(RECV_TIMEOUT)).expect("read timeout");
    stream.set_write_timeout(Some(RECV_TIMEOUT)).expect("write timeout");
    stream.write_all(bytes).expect("write malformed frame");
    stream.flush().expect("flush malformed frame");
    if partial {
        let _ = stream.shutdown(Shutdown::Write);
    }
    let message: ServerMessage = read_message(&mut stream, MAX_FRAME_LEN).expect("read rejection frame");
    match message {
        ServerMessage::Disconnect { reason } => assert!(
            reason.contains(expected),
            "reason {reason:?} does not contain {expected:?}"
        ),
        other => panic!("expected Disconnect({expected:?}), got {other:?}"),
    }
}

fn payload_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[test]
fn malformed_handshake_frames_are_rejected_with_reasons() {
    let mut server = TestServer::start("malformed-handshake", 4);

    let oversized = (MAX_FRAME_LEN as u32 + 1).to_be_bytes().to_vec();
    expect_handshake_rejection(&server, &oversized, false, "frame_too_large");
    expect_handshake_rejection(&server, &u32::MAX.to_be_bytes(), false, "frame_too_large");
    expect_handshake_rejection(&server, &[0, 0, 0, 0], false, "empty frame");
    expect_handshake_rejection(&server, &[0, 0, 0, 2, 0xff, 0xfe], false, "invalid json");
    expect_handshake_rejection(&server, &payload_frame(b"{"), false, "invalid json");
    expect_handshake_rejection(
        &server,
        &payload_frame(br#"{"type":"pong","id":1}"#),
        false,
        "protocol_error",
    );
    expect_handshake_rejection(
        &server,
        &payload_frame(br#"{"type":"unknown_type","id":1}"#),
        false,
        "protocol_error",
    );
    expect_handshake_rejection(&server, &payload_frame(br#"{"type":"ping"}"#), false, "protocol_error");
    expect_handshake_rejection(
        &server,
        &payload_frame(br#"{"type":"ping","id":-1}"#),
        false,
        "invalid json",
    );
    expect_handshake_rejection(
        &server,
        &payload_frame(br#"{"type":"ping","id":1e400}"#),
        false,
        "invalid json",
    );
    expect_handshake_rejection(
        &server,
        &payload_frame(br#"{"type":"ping","id":NaN}"#),
        false,
        "invalid json",
    );
    expect_handshake_rejection(
        &server,
        &payload_frame(br#"{"type":"chunk_request","cx":2147483648,"cz":0}"#),
        false,
        "invalid json",
    );
    expect_handshake_rejection(
        &server,
        &payload_frame(br#"{"type":"ping","id":1,"id":2}"#),
        false,
        "invalid json",
    );

    // Truncated prefix and payload: the server reports a typed reason instead
    // of silently dropping the connection.
    expect_handshake_rejection(&server, &[0, 0], true, "truncated frame");
    let mut truncated_payload = (20u32).to_be_bytes().to_vec();
    truncated_payload.extend_from_slice(b"{\"type\":");
    expect_handshake_rejection(&server, &truncated_payload, true, "truncated frame");

    // Deeply nested unknown fields hit serde's recursion limit.
    let mut nested = String::from(r#"{"type":"ping","id":1,"x":"#);
    nested.push_str(&"[".repeat(600));
    nested.push('0');
    nested.push_str(&"]".repeat(600));
    nested.push('}');
    expect_handshake_rejection(&server, &payload_frame(nested.as_bytes()), false, "invalid json");

    // A huge name is rejected after parse without allocating per-character
    // structures or panicking.
    let huge_name = ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: "a".repeat(1_000_000),
    };
    let huge = ob_protocol::encode_client_frame(&huge_name).expect("encode huge name");
    expect_handshake_rejection(&server, &huge, false, "invalid_name");

    assert_eq!(
        server.server().player_count(),
        0,
        "no malformed handshake may create a player"
    );
    server.stop();
}

#[test]
fn malformed_playing_state_frames_disconnect_without_killing_the_server() {
    let mut server = TestServer::start("malformed-playing", 4);
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (payload_frame(br#"{"type":"nope"}"#), "protocol_error"),
        (payload_frame(br#"{"type":"ping","id":1}garbage"#), "protocol_error"),
        (payload_frame(&[0xff, 0xfe]), "protocol_error"),
        (vec![0, 0, 0, 0], "empty frame"),
        (
            {
                let mut bytes = (50u32).to_be_bytes().to_vec();
                bytes.extend_from_slice(b"{\"type\":\"pi");
                bytes
            },
            "truncated frame",
        ),
        (vec![0, 1], "truncated frame"),
        (
            payload_frame(br#"{"type":"move","pos":[1e400,0,0],"yaw":0,"pitch":0}"#),
            "protocol_error",
        ),
    ];

    for (index, (bytes, expected)) in cases.into_iter().enumerate() {
        let mut client = TestClient::connect(server.addr());
        join(&server, &mut client, &format!("m{index}"));
        let mut partial = false;
        // The last two cases are deliberately incomplete frames: the server can
        // only classify them after the client closes its write side.
        if bytes == [0, 1]
            || bytes == {
                let mut b = (50u32).to_be_bytes().to_vec();
                b.extend_from_slice(b"{\"type\":\"pi");
                b
            }
        {
            partial = true;
        }
        client.send_bytes(&bytes);
        if partial {
            client.shutdown_write();
        }
        client.expect_disconnect(expected);
    }

    // The server survived every disconnect and still accepts clients.
    let mut fresh = TestClient::connect(server.addr());
    join(&server, &mut fresh, "fresh");
    fresh.send(ClientMessage::Ping { id: 7 });
    fresh.recv_until("pong after abuse", |message| {
        matches!(message, ServerMessage::Pong { id: 7 })
    });
    server.stop();
}

#[test]
fn large_fields_and_numbers_are_bounded() {
    let mut server = TestServer::start("large-fields", 4);
    let mut client = TestClient::connect(server.addr());
    let (id, _) = join(&server, &mut client, "alice");
    let start = server.server().server_player(id).expect("online").pos;

    // Oversized and control-character chat lines are dropped, not relayed.
    client.send(ClientMessage::Chat {
        text: "x".repeat(1_000_000),
    });
    client.send(ClientMessage::Chat {
        text: "line\nbreak".to_string(),
    });
    client.send(ClientMessage::Chat {
        text: "c".repeat(ob_protocol::MAX_CHAT_LEN + 1),
    });
    client.send(ClientMessage::Ping { id: 41 });
    client.recv_until("pong after bad chat", |message| {
        matches!(message, ServerMessage::Pong { id: 41 })
    });
    assert!(
        !client
            .drain_for(Duration::from_millis(250))
            .iter()
            .any(|message| matches!(
                message,
                ServerMessage::Chat { text, .. } if text.contains("line") || text.len() > ob_protocol::MAX_CHAT_LEN
            )),
        "an invalid chat line was relayed"
    );

    // Finite-but-absurd positions are clamped/rejected; the accepted position
    // never moves to the claimed value.
    client.send(ClientMessage::Move {
        pos: [1e300, 1e300, -1e300],
        yaw: 1e30,
        pitch: -1e30,
    });
    std::thread::sleep(Duration::from_millis(150));
    let player = server.server().server_player(id).expect("online");
    assert!(player.pos.is_finite());
    assert!(player.pos.distance(start) < 0.05, "a 1e300 move must not be accepted");
    assert!(player.yaw.is_finite() && player.pitch.is_finite());

    // Chunk coordinates at the extremes are ignored without overflow.
    client.send(ClientMessage::ChunkRequest {
        cx: i32::MAX,
        cz: i32::MIN,
    });
    client.send(ClientMessage::Ping { id: 42 });
    client.recv_until("pong after extreme chunk request", |message| {
        matches!(message, ServerMessage::Pong { id: 42 })
    });

    // Unknown block ids and out-of-world edits get an explicit rejection.
    let pos = BlockPos::new(
        start.x.floor() as i32 + 1,
        start.y.floor() as i32,
        start.z.floor() as i32,
    );
    client.send(ClientMessage::BlockEdit {
        pos: [pos.x, pos.y, pos.z],
        block: u16::MAX,
    });
    client.recv_until("invalid block rejection", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "block_edit" && reason == "invalid_block")
    });
    client.send(ClientMessage::SelectSlot { slot: 200 });
    client.recv_until("invalid slot rejection", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "select_slot" && reason == "invalid_slot")
    });
    client.send(ClientMessage::InventoryMove {
        from: 40,
        to: 0,
        count: 1,
        revision: None,
    });
    client.recv_until("invalid inventory slot", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && reason == "invalid_slot")
    });
    client.send(ClientMessage::InventoryMove {
        from: 0,
        to: 0,
        count: 0,
        revision: None,
    });
    client.recv_until("zero count", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && matches!(reason.as_str(), "invalid_count" | "empty_source"))
    });
    server.stop();
}

#[test]
fn admission_caps_and_idle_handshake_cleanup() {
    let mut server = TestServer::start("admission", 2);
    let mut alice = TestClient::connect(server.addr());
    let mut bob = TestClient::connect(server.addr());
    join(&server, &mut alice, "alice");
    join(&server, &mut bob, "bob");
    assert_eq!(server.server().player_count(), 2);

    let mut third = TestClient::connect(server.addr());
    third.send(ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        name: "third".to_string(),
    });
    third.expect_disconnect("server_full");
    assert_eq!(server.server().player_count(), 2);

    alice.send(ClientMessage::Disconnect);
    bob.send(ClientMessage::Disconnect);
    drop(alice);
    drop(bob);
    server.wait_player_count(0);

    // Flood the handshake stage with idle sockets. The accept loop caps
    // concurrent handshakes; closing them must release every slot again.
    let idle: Vec<TcpStream> = (0..40)
        .map(|_| TcpStream::connect(server.addr()).expect("connect idle socket"))
        .collect();
    std::thread::sleep(Duration::from_millis(800));
    for socket in &idle {
        let _ = socket.shutdown(Shutdown::Both);
    }
    drop(idle);
    // The accept loop polls every 10 ms and handshake threads unwind on EOF;
    // allow a generous margin on a loaded CI runner before reusing the slots.
    std::thread::sleep(Duration::from_millis(1_500));

    let mut fresh = TestClient::connect(server.addr());
    join(&server, &mut fresh, "fresh");
    fresh.send(ClientMessage::Ping { id: 9 });
    fresh.recv_until("pong after idle flood", |message| {
        matches!(message, ServerMessage::Pong { id: 9 })
    });
    assert_eq!(server.server().player_count(), 1);
    server.stop();
}

#[test]
fn slow_handshake_times_out_without_blocking_other_players() {
    let mut server = TestServer::start("slowloris", 4);
    let mut slow = TcpStream::connect(server.addr()).expect("connect slow client");
    slow.set_write_timeout(Some(RECV_TIMEOUT)).expect("write timeout");
    // Partial length prefix, then stall.
    slow.write_all(&[0u8, 0]).expect("write partial prefix");
    slow.flush().expect("flush partial prefix");

    // A normal client joins while the slow handshake is still pending.
    let started = Instant::now();
    let mut normal = TestClient::connect(server.addr());
    join(&server, &mut normal, "normal");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a pending handshake delayed another client: {:?}",
        started.elapsed()
    );
    assert_eq!(server.server().player_count(), 1);

    // The 10 s handshake timeout sends a machine-readable reason and closes.
    slow.set_read_timeout(Some(Duration::from_secs(13)))
        .expect("slow read timeout");
    let message: ServerMessage = read_message(&mut slow, MAX_FRAME_LEN).expect("timeout disconnect frame");
    match message {
        ServerMessage::Disconnect { reason } => assert!(reason.contains("timeout"), "{reason:?}"),
        other => panic!("expected timeout Disconnect, got {other:?}"),
    }
    assert_eq!(
        server.server().player_count(),
        1,
        "a timed-out handshake must not be a player"
    );

    normal.send(ClientMessage::Ping { id: 3 });
    normal.recv_until("pong after timeout", |message| {
        matches!(message, ServerMessage::Pong { id: 3 })
    });
    server.stop();
}

#[test]
fn chat_ping_move_edit_and_chunk_rates_are_enforced() {
    let mut server = TestServer::start("rates", 4);
    let mut client = TestClient::connect(server.addr());
    let (id, _) = join(&server, &mut client, "alice");
    let start = server.server().server_player(id).expect("online").pos;

    // Chat: 5 per 10 s, burst 5.
    for index in 0..20 {
        client.send(ClientMessage::Chat {
            text: format!("flood {index}"),
        });
    }
    let chats = client
        .drain_for(Duration::from_millis(1_500))
        .iter()
        .filter(|message| matches!(message, ServerMessage::Chat { .. }))
        .count();
    assert!((5..=6).contains(&chats), "chat limiter allowed {chats} lines");

    // Ping: 10/s, burst 10; excess pings are dropped without a disconnect.
    for id in 0..50u32 {
        client.send(ClientMessage::Ping { id });
    }
    let pongs = client
        .drain_for(Duration::from_millis(1_500))
        .iter()
        .filter(|message| matches!(message, ServerMessage::Pong { .. }))
        .count();
    assert!((10..=12).contains(&pongs), "ping limiter allowed {pongs} pings");
    client.send(ClientMessage::Ping { id: 999 });
    client.recv_until("still connected after ping flood", |message| {
        matches!(message, ServerMessage::Pong { id: 999 })
    });

    // Move: 30/s burst. A hundred 0.1 m moves must not all be applied.
    for step in 1..=100 {
        client.send(ClientMessage::Move {
            pos: [start.x + f64::from(step) * 0.1, start.y, start.z],
            yaw: 0.0,
            pitch: 0.0,
        });
    }
    std::thread::sleep(Duration::from_millis(600));
    let moved = server.server().server_player(id).expect("online").pos;
    assert!(
        (moved.x - start.x).abs() < 5.0,
        "move rate limit did not bound the distance: {}",
        moved.x - start.x
    );

    // Edit: 20/s burst. Out-of-reach edits consume tokens and are counted, so
    // sending 100 does not produce 100 validations.
    let (accepted_before, rejected_before) = server.server().edit_counts();
    let far = [
        start.x.floor() as i32 + 50,
        start.y.floor() as i32,
        start.z.floor() as i32 + 50,
    ];
    for _ in 0..100 {
        client.send(ClientMessage::BlockEdit { pos: far, block: 1 });
    }
    std::thread::sleep(Duration::from_millis(600));
    let (accepted, rejected) = server.server().edit_counts();
    assert_eq!(accepted, accepted_before, "no far edit may be accepted");
    // Every processed edit is counted as rejected, whether it failed the rate
    // limit or the reach check; the important assertion is that none were
    // accepted and that the count is bounded by the 100 frames sent.
    assert_eq!(
        rejected,
        rejected_before + 100,
        "the edit counter should account for every processed frame"
    );

    // Chunk request: 16/s burst 8. A flood, including extreme coordinates, must
    // leave the connection and tick loop healthy.
    for index in 0..100i32 {
        client.send(ClientMessage::ChunkRequest {
            cx: index.rem_euclid(3),
            cz: index / 3,
        });
    }
    client.send(ClientMessage::ChunkRequest {
        cx: i32::MIN,
        cz: i32::MAX,
    });
    client.send(ClientMessage::Ping { id: 1000 });
    client.recv_until("pong after chunk flood", |message| {
        matches!(message, ServerMessage::Pong { id: 1000 })
    });
    server.stop();
}

#[test]
fn ten_thousand_frame_spam_is_bounded_and_disconnects() {
    let mut server = TestServer::start("spam", 4);
    let mut spammer = TestClient::connect(server.addr());
    join(&server, &mut spammer, "spammer");
    spammer
        .stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .expect("short write timeout");
    let ping = ob_protocol::encode_client_frame(&ClientMessage::Ping { id: 1 }).expect("encode ping");
    let mut sent = 0usize;
    while sent < 10_000 {
        if spammer.stream.write_all(&ping).is_err() {
            break;
        }
        sent += 1;
    }
    let _ = spammer.stream.flush();
    assert!(sent > 0);
    // The inbound frame ceiling disconnects the reader thread; a reason must be
    // queued (the connection may also already be closed).
    let reason = loop {
        match read_message::<_, ServerMessage>(&mut spammer.stream, MAX_FRAME_LEN) {
            Ok(ServerMessage::Disconnect { reason }) => break reason,
            Ok(_) => continue,
            Err(_) => break String::new(),
        }
    };
    assert!(
        reason.contains("rate_limited") || reason.is_empty(),
        "unexpected reason {reason:?}"
    );

    server.wait_player_count(0);
    let mut fresh = TestClient::connect(server.addr());
    join(&server, &mut fresh, "fresh");
    fresh.send(ClientMessage::Ping { id: 5 });
    fresh.recv_until("server survived spam", |message| {
        matches!(message, ServerMessage::Pong { id: 5 })
    });
    server.stop();
}

#[test]
fn survival_authority_rejects_invalid_or_dead_intents() {
    let mut server = TestServer::start("authority", 4);
    let mut client = TestClient::connect(server.addr());
    let (id, _) = join(&server, &mut client, "alice");
    let before = server.server().server_player(id).expect("online").inventory;

    // Dead players cannot mutate state.
    assert!(server.server().set_network_player_vitals(id, 0.0, 20.0));
    let deadline = Instant::now() + WAIT;
    while !server.server().server_player(id).expect("online").dead {
        assert!(Instant::now() < deadline, "player never died");
        std::thread::sleep(Duration::from_millis(10));
    }
    let pos = server.server().server_player(id).expect("online").pos;
    let edit_pos = [pos.x.floor() as i32 + 1, pos.y.floor() as i32, pos.z.floor() as i32];
    client.send(ClientMessage::BlockEdit {
        pos: edit_pos,
        block: 1,
    });
    client.recv_until("dead edit", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "block_edit" && reason == "dead")
    });
    client.send(ClientMessage::SelectSlot { slot: 0 });
    client.recv_until("dead select", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "select_slot" && reason == "dead")
    });
    client.send(ClientMessage::UseBlock { pos: edit_pos });
    client.recv_until("dead use", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "use_block" && reason == "dead")
    });
    client.send(ClientMessage::InventoryMove {
        from: 0,
        to: 1,
        count: 1,
        revision: None,
    });
    client.recv_until("dead inventory", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && reason == "dead")
    });
    client.send(ClientMessage::Equip { hotbar_slot: 0 });
    client.recv_until("dead equip", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "equip" && reason == "dead")
    });
    client.send(ClientMessage::Attack { target: Some(1) });
    client.recv_until("dead attack", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "attack" && reason == "dead")
    });
    // Eating while dead is rejected by the shared survival rule.
    client.send(ClientMessage::Eat);
    client.recv_until(
        "dead eat",
        |message| matches!(message, ServerMessage::ActionRejected { action, .. } if action == "eat"),
    );
    assert_eq!(
        server.server().server_player(id).expect("online").inventory,
        before,
        "dead actions must not change the inventory"
    );

    // Respawn restores actionability and keeps the inventory.
    client.send(ClientMessage::Respawn);
    let respawn = client.recv_until("respawn", |message| matches!(message, ServerMessage::Respawn { .. }));
    assert!(matches!(respawn, ServerMessage::Respawn { .. }));
    let player = server.server().server_player(id).expect("online");
    assert!(!player.dead);
    assert_eq!(player.inventory, before);

    // Unknown/fake/self attack ids are rejected without touching the player.
    for target in [0u64, 1, u64::MAX] {
        client.send(ClientMessage::Attack { target: Some(target) });
        client.recv_until("invalid attack target", |message| {
            matches!(message, ServerMessage::ActionRejected { action, reason } if action == "attack" && reason == "no_target")
        });
    }
    assert_eq!(server.server().server_player(id).expect("online").health, 20.0);

    // Inventory fuzz over the real network: every authoritative stack stays
    // valid and the per-item totals never change, even though the rate limiter
    // drops most of the flood.
    assert!(server.server().give_network_item(id, "openblock:stone", 32));
    assert!(server.server().give_network_item(id, "openblock:apple", 4));
    assert!(server.server().give_network_item(id, "openblock:wooden_pickaxe", 1));
    let totals = |server: &TestServer| {
        let mut totals = std::collections::BTreeMap::<u16, u32>::new();
        let player = server.server().server_player(id).expect("online");
        for stack in player.inventory.iter().chain(player.equipment.iter()).flatten() {
            *totals.entry(stack.item.0).or_insert(0) += u32::from(stack.count);
        }
        totals
    };
    let before_fuzz = totals(&server);
    let mut state = 0x1234_5678_9abc_def0u64;
    for _ in 0..120 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let from = (state % 14) as u16;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let to = (state % 14) as u16;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let count = [0u8, 1, 2, 4, 32, 255][(state % 6) as usize];
        client.send(ClientMessage::InventoryMove {
            from,
            to,
            count,
            revision: None,
        });
    }
    client.send(ClientMessage::Ping { id: 77 });
    client.recv_until("pong after inventory fuzz", |message| {
        matches!(message, ServerMessage::Pong { id: 77 })
    });
    assert_eq!(totals(&server), before_fuzz, "inventory fuzz changed the item totals");
    server.stop();
}

#[test]
fn entity_replication_respects_interest_and_ignores_client_ids() {
    // Alice restores 1000 m away; Bob joins at spawn. A mob spawned next to Bob
    // must never be replicated to Alice.
    let saved = "alice 1000 100 0 0 0 20 20 0 0 survival\n";
    let mut server = TestServer::start_with_saved("entity-interest", 4, Some(saved));
    let mut alice = TestClient::connect(server.addr());
    let (alice_id, alice_entities) = join(&server, &mut alice, "alice");
    assert_eq!(alice_entities, 0, "a far spawn area should have no entities");
    let alice_pos = server.server().server_player(alice_id).expect("online").pos;
    assert!(
        (alice_pos.x - 1000.0).abs() < 0.01,
        "saved position was not restored: {alice_pos:?}"
    );

    let mut bob = TestClient::connect(server.addr());
    let (_bob_id, _) = join(&server, &mut bob, "bob");
    let spawn = server.spawn;
    let mob_id = server
        .server()
        .spawn_mob(MobKind::Zombie, [spawn.x + 1.0, spawn.y, spawn.z + 1.0])
        .expect("spawn test mob");
    let spawn_message = bob.recv_until("nearby mob spawn", |message| match message {
        ServerMessage::EntitySpawn { id, .. } => *id == mob_id,
        _ => false,
    });
    assert!(matches!(spawn_message, ServerMessage::EntitySpawn { .. }));

    // Alice's stream carries chunks and player states; none may mention the mob.
    let leaked = alice
        .drain_for(Duration::from_millis(800))
        .into_iter()
        .filter(|message| match message {
            ServerMessage::EntitySpawn { id, .. } => *id == mob_id,
            ServerMessage::EntityState { entities } => entities.iter().any(|entity| entity.id == mob_id),
            ServerMessage::EntitySnapshot { entities } => entities.iter().any(|entity| entity.id == mob_id),
            ServerMessage::EntityDespawn { ids, .. } => ids.contains(&mob_id),
            _ => false,
        })
        .count();
    assert_eq!(
        leaked, 0,
        "entity replication leaked a mob 1000 m outside the interest radius"
    );

    // A client cannot inject entities by naming a server message type.
    alice.send_payload(
        br#"{"type":"entity_spawn","id":1,"kind":{"kind":"mob","mob":"openblock:zombie"},"pos":[1000,100,0],"yaw":0,"health":20,"hurt":false}"#,
    );
    alice.expect_disconnect("protocol_error");

    // Referencing a real but far entity id is a no-op for Alice (reach check).
    let mut alice_again = TestClient::connect(server.addr());
    join(&server, &mut alice_again, "alice");
    alice_again.send(ClientMessage::Attack { target: Some(mob_id) });
    let result = alice_again.recv_until(
        "far attack result",
        |message| matches!(message, ServerMessage::AttackResult { target, .. } if *target == mob_id),
    );
    assert!(matches!(
        result,
        ServerMessage::AttackResult {
            target,
            hit: false,
            damage,
            killed: false,
        } if target == mob_id && damage == 0.0
    ));
    server.stop();
}

#[test]
fn inventory_window_authority_and_output_slot_are_per_connection() {
    // A player without an open window cannot address window slots, including a
    // guessed window 1. Bob's no-window rejection covers the "other player's
    // window" case because windows are stored per connection, never globally.
    let mut server = TestServer::start("window-authority", 4);
    let mut alice = TestClient::connect(server.addr());
    let mut bob = TestClient::connect(server.addr());
    let (alice_id, _) = join(&server, &mut alice, "alice");
    let (bob_id, _) = join(&server, &mut bob, "bob");
    assert!(server.server().give_network_item(alice_id, "openblock:chest", 1));
    assert!(server.server().give_network_item(alice_id, "openblock:coal", 3));
    let chest_slot = {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(player) = server.server().server_player(alice_id)
                && let Some(slot) = player.inventory.iter().position(|stack| {
                    stack.is_some_and(|stack| stack.item == server.registry.item_id("openblock:chest").unwrap())
                })
            {
                break slot as u8;
            }
            assert!(Instant::now() < deadline, "chest never entered the inventory");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let coal_slot = {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(player) = server.server().server_player(alice_id)
                && let Some(slot) = player.inventory.iter().position(|stack| {
                    stack.is_some_and(|stack| stack.item == server.registry.item_id("openblock:coal").unwrap())
                })
            {
                break slot as u8;
            }
            assert!(Instant::now() < deadline, "coal never entered the inventory");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    // Place and open a chest using the same public intents as a client.
    let chest_block = server.registry.id("openblock:chest").expect("chest");
    alice.send(ClientMessage::SelectSlot { slot: chest_slot });
    let start = server.server().server_player(alice_id).expect("online").pos;
    let mut target = None;
    let deadline = Instant::now() + WAIT;
    while target.is_none() {
        let base = BlockPos::new(start.x.floor() as i32, start.y.floor() as i32, start.z.floor() as i32);
        for dy in 0..=2 {
            for dz in -2..=2 {
                for dx in -2..=2 {
                    let pos = base.offset(dx, dy, dz);
                    if server.server().block_at(pos) == Some(ob_content::BlockId::AIR) {
                        let supported = [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)]
                            .into_iter()
                            .any(|(x, y, z)| {
                                server
                                    .server()
                                    .block_at(pos.offset(x, y, z))
                                    .is_some_and(|block| block != ob_content::BlockId::AIR)
                            });
                        if supported {
                            target = Some(pos);
                            break;
                        }
                    }
                }
            }
        }
        assert!(Instant::now() < deadline, "no chest placement target");
        std::thread::sleep(Duration::from_millis(20));
    }
    let target = target.expect("placement target");
    alice.send(ClientMessage::BlockEdit {
        pos: [target.x, target.y, target.z],
        block: chest_block.0,
    });
    alice.recv_until(
        "chest placed",
        |message| matches!(message, ServerMessage::BlockEdit { pos, .. } if *pos == [target.x, target.y, target.z]),
    );
    alice.send(ClientMessage::UseBlock {
        pos: [target.x, target.y, target.z],
    });
    let opened = alice.recv_until("chest open", |message| {
        matches!(
            message,
            ServerMessage::ContainerOpen {
                kind: ob_protocol::ContainerKind::Chest,
                ..
            }
        )
    });
    let ServerMessage::ContainerOpen { revision, .. } = opened else {
        unreachable!()
    };

    // Bob has no window: any window address is rejected even with a guessed id.
    bob.send(ClientMessage::InventoryMove {
        from: u16::from(coal_slot),
        to: 100,
        count: 1,
        revision: Some(revision),
    });
    bob.recv_until("bob no window", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && reason == "no_window")
    });
    assert_eq!(
        server.server().server_player(bob_id).expect("online").inventory,
        [None; ob_server::INVENTORY_SLOTS]
    );

    // Alice's window: a present stale revision is rejected.
    alice.send(ClientMessage::InventoryMove {
        from: u16::from(coal_slot),
        to: 100,
        count: 1,
        revision: Some(revision + 1),
    });
    alice.recv_until("future revision", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && reason == "stale_revision")
    });
    // Count 0 and an impossible count are rejected.
    alice.send(ClientMessage::InventoryMove {
        from: u16::from(coal_slot),
        to: 100,
        count: 0,
        revision: None,
    });
    alice.recv_until("zero move count", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && reason == "invalid_count")
    });
    alice.send(ClientMessage::InventoryMove {
        from: u16::from(coal_slot),
        to: 100,
        count: 255,
        revision: None,
    });
    alice.recv_until("oversized move count", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && reason == "invalid_count")
    });
    // A far window slot is rejected.
    alice.send(ClientMessage::InventoryMove {
        from: 999,
        to: 0,
        count: 1,
        revision: None,
    });
    alice.recv_until("out-of-range window slot", |message| {
        matches!(message, ServerMessage::ActionRejected { action, reason } if action == "inventory_move" && reason == "invalid_slot")
    });
    server.stop();
}

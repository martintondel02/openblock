//! Linux-only file-descriptor and thread leak regression for connection storms.
//!
//! This lives in its own integration-test binary so `/proc/self/fd` and
//! `/proc/self/task` only contain this test's server and clients; other test
//! binaries run in separate processes.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_content::Registry;
use ob_protocol::{ClientMessage, MAX_FRAME_LEN, PROTOCOL_VERSION, ServerMessage, read_message, write_message};
use ob_server::{DedicatedServer, GameMode, Host, HostConfig, NetConfig};

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
            "openblock-net-fd-{label}-{}-{nanos}-{unique}",
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

fn socket_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("read /proc/self/fd")
        .filter_map(Result::ok)
        .filter(|entry| {
            std::fs::read_link(entry.path())
                .map(|target| target.to_string_lossy().starts_with("socket:"))
                .unwrap_or(false)
        })
        .count()
}

fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("read /proc/self/task")
        .filter_map(Result::ok)
        .count()
}

fn recv_until(stream: &mut TcpStream, what: &str, mut predicate: impl FnMut(&ServerMessage) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        let message: ServerMessage = read_message(stream, MAX_FRAME_LEN).expect("read server frame");
        if predicate(&message) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
    }
}

fn join(stream: &mut TcpStream, name: &str) {
    write_message(
        stream,
        &ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            name: name.to_string(),
        },
    )
    .expect("write hello");
    stream.flush().expect("flush hello");
    recv_until(stream, "hello", |message| {
        matches!(message, ServerMessage::Hello { .. })
    });
    recv_until(
        stream,
        "join",
        |message| matches!(message, ServerMessage::Join { name: joined, .. } if joined == name),
    );
    recv_until(stream, "player list", |message| {
        matches!(message, ServerMessage::PlayerList { .. })
    });
    recv_until(stream, "entity snapshot", |message| {
        matches!(message, ServerMessage::EntitySnapshot { .. })
    });
}

fn wait_players(server: &DedicatedServer, want: usize) {
    let deadline = Instant::now() + WAIT;
    while server.player_count() != want {
        assert!(
            Instant::now() < deadline,
            "server stayed at {} players instead of {want}",
            server.player_count()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn reconnect_storm_does_not_leak_sockets_or_threads() {
    let dir = TempDir::new("storm");
    let registry = Arc::new(Registry::base());
    let host_config = HostConfig {
        seed: 7,
        world_name: "fd-storm".to_string(),
        data_dir: Some(dir.path().to_path_buf()),
        render_distance_chunks: 2,
        generate_budget: 8,
    };
    let host = Host::open(registry, host_config).expect("open test world");
    let net_config = NetConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().expect("ephemeral loopback"),
        max_players: 4,
        motd: "fd storm".to_string(),
        interest_radius_chunks: 2,
        chunks_per_tick: 4,
        save_interval_seconds: 0,
        whitelist: Vec::new(),
        game_mode: GameMode::Survival,
    };
    let server = DedicatedServer::start(host, net_config).expect("start server");
    let addr = server.local_addr();
    std::thread::sleep(Duration::from_millis(100));
    let baseline_fds = socket_fds();
    let baseline_threads = thread_count();

    // Two full turns of the player cap, alternating an abrupt drop with a
    // clean Disconnect so both reader-EOF and writer-flush paths are covered.
    for round in 0..8 {
        let name = format!("storm{round}");
        let mut client = TcpStream::connect(addr).expect("connect storm client");
        client.set_read_timeout(Some(WAIT)).expect("read timeout");
        client.set_write_timeout(Some(WAIT)).expect("write timeout");
        join(&mut client, &name);
        if round % 2 == 0 {
            // Abrupt close: the reader sees EOF and the tick removes the player.
            client.shutdown(std::net::Shutdown::Both).expect("abrupt shutdown");
        } else {
            write_message(&mut client, &ClientMessage::Disconnect).expect("write disconnect");
            client.flush().expect("flush disconnect");
        }
        drop(client);
        wait_players(&server, 0);
    }

    // Reader and writer threads are detached by design; they must still exit
    // promptly, so poll until the descriptors and threads settle.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let fds = socket_fds();
        let threads = thread_count();
        if fds <= baseline_fds + 1 && threads <= baseline_threads + 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "leak after reconnect storm: fds {} -> {fds}, threads {} -> {threads}",
            baseline_fds,
            baseline_threads
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut healthy = TcpStream::connect(addr).expect("connect after storm");
    healthy.set_read_timeout(Some(WAIT)).expect("read timeout");
    healthy.set_write_timeout(Some(WAIT)).expect("write timeout");
    join(&mut healthy, "healthy");
    write_message(&mut healthy, &ClientMessage::Ping { id: 1 }).expect("write ping");
    healthy.flush().expect("flush ping");
    recv_until(&mut healthy, "pong", |message| {
        matches!(message, ServerMessage::Pong { id: 1 })
    });
    drop(healthy);
    wait_players(&server, 0);
    server.stop().expect("stop saves the world");
}

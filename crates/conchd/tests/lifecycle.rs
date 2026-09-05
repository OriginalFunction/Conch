use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::Duration,
};

use conch_core::{
    client::{ClientReply, ClientRequest},
    frame,
    types::AgentId,
};
use conch_launch::{spawn_detached, PidFile, SpawnOptions};
use conchd::tcp::Daemon;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Two loopback ports for a daemon this test is about to start.
///
/// They come from below the ephemeral range (macOS hands out 49152 and up), because an
/// ephemeral port is offered to unrelated processes the instant the reservation is
/// released — and it has to be released before conchd can bind it. The cursor only ever
/// walks forward, so no port is issued twice in this binary; its starting band is
/// derived from both the clock and the pid, so binaries running side by side do not
/// overlap and a port is not offered again for minutes. The listeners stay bound until
/// the caller drops them, immediately before the daemon runs.
fn reserve_ports() -> (SocketAddr, SocketAddr, (TcpListener, TcpListener)) {
    static NEXT: Mutex<Option<u16>> = Mutex::new(None);
    let mut next = NEXT.lock().expect("port cursor is not poisoned");
    let cursor = next.get_or_insert_with(|| {
        // The band advances with the clock as well as the pid, so a port is not offered
        // again for several minutes. A daemon that has just been stopped can leave a
        // connection endpoint on its listening port for tens of seconds, and that
        // outlives a scheme that only cycles through a few bands.
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_secs();
        20_000 + ((seconds + std::process::id() as u64) % 600) as u16 * 30
    });
    let mut chosen = Vec::new();
    while chosen.len() < 2 {
        let candidate = *cursor;
        *cursor = if candidate >= 38_999 {
            20_000
        } else {
            candidate + 1
        };
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", candidate)) {
            chosen.push(listener);
        }
    }
    let http = chosen.pop().expect("two ports");
    let tcp = chosen.pop().expect("two ports");
    let addrs = (tcp.local_addr().unwrap(), http.local_addr().unwrap());
    (addrs.0, addrs.1, (tcp, http))
}

/// Reserve a fresh pair of ports and hand them to `start`, retrying when the daemon
/// could not bind one of them.
///
/// The reservation has to be released before conchd can take the port, and in that
/// window anything on the machine may claim it — the daemon then exits with "address
/// already in use". That is a property of the harness, not of the code under test, so
/// the test picks another pair rather than failing.
fn with_daemon_ports(
    mut start: impl FnMut(SocketAddr, SocketAddr) -> bool,
) -> (SocketAddr, SocketAddr) {
    for attempt in 1..=4 {
        let (tcp, http, reserved) = reserve_ports();
        drop(reserved);
        if start(tcp, http) {
            return (tcp, http);
        }
        assert!(attempt < 4, "conchd could not bind any reserved port pair");
    }
    unreachable!("the loop either returns or asserts")
}

/// Stops whatever the pid file names when the test ends, so a failing assertion
/// never leaks a daemon.
struct DaemonGuard(PathBuf);

impl DaemonGuard {
    fn new(data_dir: &Path) -> Self {
        Self(data_dir.to_path_buf())
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = PidFile::read(&self.0) {
            let _ = pid.stop(Duration::from_secs(5));
        }
    }
}

async fn request(addr: SocketAddr, request: &ClientRequest) -> ClientReply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    for message in [
        &ClientRequest::Attach {
            agent: AgentId::new("agent:test").unwrap(),
        },
        request,
    ] {
        stream
            .write_all(&frame::encode(message).unwrap())
            .await
            .unwrap();
        let length = stream.read_u32().await.unwrap() as usize;
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).await.unwrap();
        let reply: ClientReply = frame::decode_payload(&payload).unwrap();
        if matches!(message, ClientRequest::Attach { .. }) {
            assert!(reply.ok);
        } else {
            return reply;
        }
    }
    unreachable!()
}

#[tokio::test]
async fn version_request_reports_daemon_version() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let reply = request(server.addr(), &ClientRequest::Version).await;
    assert!(reply.ok);
    assert_eq!(reply.data.unwrap()["version"], env!("CARGO_PKG_VERSION"));
    server.abort();
}

#[tokio::test]
async fn a_daemon_that_cannot_bind_leaves_the_running_daemon_alone() {
    let data = TempDir::new().unwrap();
    let _guard = DaemonGuard::new(data.path());
    let mut first = 0;
    let (tcp, http) = with_daemon_ports(|tcp, http| {
        let options = SpawnOptions {
            conchd: PathBuf::from(env!("CARGO_BIN_EXE_conchd")),
            data_dir: data.path().to_path_buf(),
            tcp,
            http,
        };
        match spawn_detached(&options) {
            Ok(pid) => {
                first = pid;
                true
            }
            Err(_) => false,
        }
    });

    // A second daemon aimed at the same data dir and ports cannot bind.
    let second = Command::new(env!("CARGO_BIN_EXE_conchd"))
        .arg("--localhost")
        .arg("--data-dir")
        .arg(data.path())
        .arg("--tcp")
        .arg(tcp.to_string())
        .arg("--http")
        .arg(http.to_string())
        .output()
        .unwrap();
    assert!(
        !second.status.success(),
        "second daemon should refuse to start: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // It must not have taken over, or deleted, the running daemon's pid file.
    let file = PidFile::read(data.path()).expect("first daemon's pid file survives");
    assert_eq!(file.pid, first);
    assert!(file.is_alive());
    let reply = request(tcp, &ClientRequest::Version).await;
    assert!(reply.ok);
    assert_eq!(reply.data.unwrap()["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn a_daemon_rebinds_its_port_straight_after_a_restart() {
    // What `conch down && conch up` does. A client connection leaves a socket in
    // TIME_WAIT holding the listening port after the daemon exits, so the replacement
    // cannot bind unless the listener asks for SO_REUSEADDR.
    let data = TempDir::new().unwrap();
    let _guard = DaemonGuard::new(data.path());
    let (tcp, http) = with_daemon_ports(|tcp, http| {
        spawn_detached(&SpawnOptions {
            conchd: PathBuf::from(env!("CARGO_BIN_EXE_conchd")),
            data_dir: data.path().to_path_buf(),
            tcp,
            http,
        })
        .is_ok()
    });
    let options = SpawnOptions {
        conchd: PathBuf::from(env!("CARGO_BIN_EXE_conchd")),
        data_dir: data.path().to_path_buf(),
        tcp,
        http,
    };
    // Still connected when the daemon goes: the daemon closes first, which is what
    // leaves its own listening port in TIME_WAIT.
    let held = std::net::TcpStream::connect(tcp).unwrap();
    PidFile::read(data.path())
        .unwrap()
        .stop(Duration::from_secs(5))
        .unwrap();
    drop(held);

    spawn_detached(&options).expect("a restarted daemon rebinds its own port");
}

#[test]
fn daemon_binary_writes_pid_file_and_removes_it_on_sigterm() {
    let data = TempDir::new().unwrap();
    let _guard = DaemonGuard::new(data.path());
    let mut pid = 0;
    let (tcp, http) = with_daemon_ports(|tcp, http| {
        match spawn_detached(&SpawnOptions {
            conchd: PathBuf::from(env!("CARGO_BIN_EXE_conchd")),
            data_dir: data.path().to_path_buf(),
            tcp,
            http,
        }) {
            Ok(started) => {
                pid = started;
                true
            }
            Err(_) => false,
        }
    });
    let options = SpawnOptions {
        conchd: PathBuf::from(env!("CARGO_BIN_EXE_conchd")),
        data_dir: data.path().to_path_buf(),
        tcp,
        http,
    };
    let file = PidFile::read(data.path()).expect("pid file written once listeners are bound");
    assert_eq!(file.pid, pid);
    assert_eq!(file.tcp, options.tcp);
    assert!(file.is_alive());
    file.stop(Duration::from_secs(5)).unwrap();
    assert!(
        PidFile::read(data.path()).is_none(),
        "pid file removed on clean shutdown"
    );
}

#[tokio::test]
async fn status_for_a_room_names_it() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "Doctor Room",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(30),
        )
        .unwrap();
    let server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let reply = request(
        server.addr(),
        &ClientRequest::Status {
            room: Some(ticket.id),
        },
    )
    .await;
    assert!(reply.ok);
    let data = reply.data.unwrap();
    assert_eq!(data["name"], "Doctor Room");
    assert_eq!(data["head_n"], 0);
    server.abort();
}

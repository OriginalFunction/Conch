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
use serde_json::json;
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

#[tokio::test]
async fn history_records_name_the_author_of_each_take() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "Authors",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(300),
        )
        .unwrap();
    let server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let room = ticket.id;
    // One connection per request: `request` attaches as agent:test each time.
    let granted = request(
        server.addr(),
        &ClientRequest::WaitForFloor {
            room,
            timeout_secs: Some(5),
        },
    )
    .await;
    assert!(granted.ok, "{granted:?}");
    let spoke = request(
        server.addr(),
        &ClientRequest::Speak {
            room,
            text: "first take".into(),
            request_id: "00000000000000000000000000000001".into(),
        },
    )
    .await;
    assert!(spoke.ok, "{spoke:?}");
    let yielded = request(server.addr(), &ClientRequest::Yield { room }).await;
    assert!(yielded.ok, "{yielded:?}");
    // A vacant configuration change has no author.
    let configured = request(
        server.addr(),
        &ClientRequest::Membership {
            room,
            stake: None,
            floor: Some(conch_core::types::FloorConfig::stick(120)),
            timeout_secs: None,
        },
    )
    .await;
    assert!(configured.ok, "{configured:?}");

    let page = request(
        server.addr(),
        &ClientRequest::History {
            room,
            from_n: 0,
            follow: false,
        },
    )
    .await;
    assert!(page.ok, "{page:?}");
    let scenes = page.data.unwrap()["scenes"].as_array().unwrap().clone();
    assert_eq!(scenes.len(), 4, "{scenes:?}");
    assert_eq!(scenes[0]["scene"]["body"]["type"], "genesis");
    assert!(scenes[0].get("author").is_none());
    assert_eq!(scenes[1]["scene"]["body"]["type"], "grant");
    assert!(
        scenes[1].get("author").is_none(),
        "grants say `to`, not author"
    );
    assert_eq!(scenes[2]["scene"]["body"]["type"], "speech");
    assert_eq!(scenes[2]["author"]["agent"], "agent:test");
    assert_eq!(scenes[2]["author"]["node"], json!(daemon.node_id()));
    assert_eq!(scenes[3]["scene"]["body"]["type"], "membership");
    assert!(
        scenes[3].get("author").is_none(),
        "vacant membership has no author"
    );

    // A page that starts after the grant still resolves the author.
    let tail = request(
        server.addr(),
        &ClientRequest::History {
            room,
            from_n: 2,
            follow: false,
        },
    )
    .await;
    let scenes = tail.data.unwrap()["scenes"].as_array().unwrap().clone();
    assert_eq!(scenes[0]["author"]["agent"], "agent:test");
    server.abort();
}

#[tokio::test]
async fn status_reports_holder_queue_and_participants() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "Who",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(300),
        )
        .unwrap();
    let server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let room = ticket.id;

    // agent:test takes the floor; a second mouth queues behind it.
    assert!(
        request(
            server.addr(),
            &ClientRequest::WaitForFloor {
                room,
                timeout_secs: Some(5)
            }
        )
        .await
        .ok
    );
    let mut second = TcpStream::connect(server.addr()).await.unwrap();
    for message in [
        &ClientRequest::Attach {
            agent: AgentId::new("agent:second").unwrap(),
        },
        &ClientRequest::RaiseHand { room },
    ] {
        second
            .write_all(&frame::encode(message).unwrap())
            .await
            .unwrap();
        let length = second.read_u32().await.unwrap() as usize;
        let mut payload = vec![0; length];
        second.read_exact(&mut payload).await.unwrap();
        let reply: ClientReply = frame::decode_payload(&payload).unwrap();
        assert!(reply.ok, "{reply:?}");
    }

    let reply = request(server.addr(), &ClientRequest::Status { room: Some(room) }).await;
    assert!(reply.ok, "{reply:?}");
    let data = reply.data.unwrap();
    assert_eq!(data["name"], "Who");
    assert_eq!(data["mode"], "stick");
    assert_eq!(data["timeout_secs"], 300);
    assert_eq!(data["holder"]["agent"], "agent:test");
    assert_eq!(data["holder"]["since_n"], 1);
    assert!(data["holder"]["granted_ts"].as_u64().unwrap() > 1_700_000_000);
    assert_eq!(data["holder"]["grant_hash"].as_str().unwrap().len(), 64);
    let queue = data["queue"].as_array().unwrap();
    assert_eq!(queue.len(), 1, "{queue:?}");
    assert_eq!(queue[0]["agent"], "agent:second");
    assert_eq!(queue[0]["kind"], "raise");
    assert_eq!(data["participants"], json!(["agent:second", "agent:test"]));

    assert!(
        request(server.addr(), &ClientRequest::Yield { room })
            .await
            .ok
    );
    // The stick passes to agent:second on its own; wait for that grant.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let replay = daemon.replay(room).unwrap();
            if replay
                .chain
                .live_grant
                .as_ref()
                .is_some_and(|g| g.to.agent.as_str() == "agent:second")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let data = request(server.addr(), &ClientRequest::Status { room: Some(room) })
        .await
        .data
        .unwrap();
    assert_eq!(data["holder"]["agent"], "agent:second");
    assert_eq!(data["queue"].as_array().unwrap().len(), 0);
    server.abort();
}

#[tokio::test]
async fn status_without_a_room_lists_room_summaries() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let _first = daemon
        .create_ticket(
            "First",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(300),
        )
        .unwrap();
    let second = daemon
        .create_ticket(
            "Second",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(300),
        )
        .unwrap();
    let _third = daemon
        .create_ticket(
            "Third",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(300),
        )
        .unwrap();
    let server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    // Activity in the second room makes it the most recent: wait for floor and speak.
    let wait_reply = request(
        server.addr(),
        &ClientRequest::WaitForFloor {
            room: second.id,
            timeout_secs: Some(5),
        },
    )
    .await;
    assert!(wait_reply.ok, "{wait_reply:?}");
    // Add a take record to the second room to ensure it has newer activity.
    let speak_reply = request(
        server.addr(),
        &ClientRequest::Speak {
            room: second.id,
            text: "activity".to_string(),
            request_id: "00000000000000000000000000000001".into(),
        },
    )
    .await;
    assert!(speak_reply.ok, "{speak_reply:?}");

    let reply = request(server.addr(), &ClientRequest::Status { room: None }).await;
    assert!(reply.ok, "{reply:?}");
    let rooms = reply.data.unwrap()["rooms"].as_array().unwrap().clone();
    assert_eq!(rooms.len(), 3);
    // Verify sort order: most recent first, then by ascending id for ties.
    assert!(
        rooms[0]["last_activity"].as_u64().unwrap() >= rooms[1]["last_activity"].as_u64().unwrap(),
        "room at [0] should have last_activity >= [1]"
    );
    assert!(
        rooms[1]["last_activity"].as_u64().unwrap() >= rooms[2]["last_activity"].as_u64().unwrap(),
        "room at [1] should have last_activity >= [2]"
    );
    // Find Second room (which had WaitForFloor + Speak) and verify it has activity.
    let second_room = rooms
        .iter()
        .find(|r| r["name"] == "Second")
        .expect("Second room should be present");
    assert_eq!(second_room["id"], json!(second.id));
    assert_eq!(
        second_room["head_n"], 1,
        "Second should have activity (take record from Speak)"
    );
    assert_eq!(
        second_room["holder"]["agent"], "agent:test",
        "Second should have holder set (floor granted)"
    );
    assert_eq!(second_room["role"], "stake");
    // Verify that rooms are sorted correctly: most recent first, then by id for ties.
    let second_ts = rooms[0]["last_activity"].as_u64().unwrap();
    let third_ts = rooms[1]["last_activity"].as_u64().unwrap();
    if second_ts == third_ts {
        let second_id = rooms[0]["id"].as_str().unwrap();
        let third_id = rooms[1]["id"].as_str().unwrap();
        assert!(
            second_id <= third_id,
            "When tied on timestamp, rooms should be sorted by ascending id"
        );
    }
    // Verify genesis-only rooms have null holder.
    let genesis_only_rooms: Vec<_> = rooms.iter().filter(|r| r["head_n"] == 0).collect();
    assert!(
        !genesis_only_rooms.is_empty(),
        "Should have at least one genesis-only room"
    );
    for room in genesis_only_rooms {
        assert_eq!(room["holder"], serde_json::Value::Null);
    }
    server.abort();
}

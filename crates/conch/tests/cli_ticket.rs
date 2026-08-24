use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use conch_core::ticket::{JoinRole, Ticket};
use conchd::tcp::Daemon;
use serde_json::Value;
use tempfile::TempDir;
use tokio::process::Command;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_writes_slug_ticket_and_prints_pinned_magnet() {
    let data = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let node = format!("tcp://{}", server.addr());

    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["--node", &node, "create", "--name", "My Design Room!"])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["ticket_path"], "./my-design-room.conch");
    let magnet = reply["magnet"].as_str().unwrap();
    assert!(magnet.starts_with("conch:1:"));
    assert!(magnet.contains("&g="));

    let bytes = fs::read(output_dir.path().join("my-design-room.conch")).unwrap();
    let ticket = Ticket::from_json_slice(&bytes).unwrap();
    assert_eq!(ticket.id.to_string(), reply["id"].as_str().unwrap());
    assert_eq!(Ticket::from_magnet(magnet).unwrap().genesis, ticket.genesis);
    assert_eq!(ticket.peers, vec![node]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_output_and_captured_errors_never_log_the_room_capability() {
    let data = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let node = format!("tcp://{}", server.addr());
    let sentinel = "a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5";

    let created = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args([
            "--node",
            &node,
            "--token",
            sentinel,
            "create",
            "--name",
            "Sentinel Secret",
        ])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(created.status.success());
    assert!(!String::from_utf8_lossy(&created.stdout).contains(sentinel));
    assert!(!String::from_utf8_lossy(&created.stderr).contains(sentinel));
    let reply: Value = serde_json::from_slice(&created.stdout).unwrap();
    assert!(!reply["magnet"].as_str().unwrap().contains("x.cap="));
    let ticket = Ticket::from_json_slice(
        &fs::read(output_dir.path().join("sentinel-secret.conch")).unwrap(),
    )
    .unwrap();
    assert_eq!(ticket.token.unwrap().to_string(), sentinel);

    let rejected = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args([
            "--node",
            &node,
            "--token",
            sentinel,
            "create",
            "--name",
            "Rejected Secret",
            "--open",
        ])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!String::from_utf8_lossy(&rejected.stdout).contains(sentinel));
    assert!(!String::from_utf8_lossy(&rejected.stderr).contains(sentinel));
}

#[tokio::test]
async fn create_observe_rejected_before_connecting() {
    let output_dir = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["create", "--name", "invalid", "--observe"])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("create cannot use --observe"));
    assert!(!output_dir.path().join("invalid.conch").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_file_defaults_to_stake_and_fetches_verified_genesis() {
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let follower_server = follower.start(loopback()).await.unwrap();
    let source_node = format!("tcp://{}", source_server.addr());
    let follower_node = format!("tcp://{}", follower_server.addr());
    let binary = env!("CARGO_BIN_EXE_conch");

    let created = Command::new(binary)
        .args(["--node", &source_node, "create", "--name", "Join Test"])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(created.status.success());
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    let room = created["id"].as_str().unwrap();

    let joined = Command::new(binary)
        .args(["--node", &follower_node, "join", "join-test.conch"])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stderr)
    );
    let reply: Value = serde_json::from_slice(&joined.stdout).unwrap();
    assert_eq!(reply["id"], room);
    assert_eq!(
        reply["role"],
        serde_json::to_value(JoinRole::Stake).unwrap()
    );

    let room = room.parse().unwrap();
    assert_eq!(
        follower.replay(room).unwrap().history,
        source.replay(room).unwrap().history
    );
    let local_join: Value = serde_json::from_slice(
        &fs::read(
            follower_data
                .path()
                .join("rooms")
                .join(room.to_string())
                .join("join.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(local_join["role"], "stake");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_http_ticket_uses_the_same_parser_and_fetches_genesis() {
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_tcp = source.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket(
            "HTTP Join",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(30),
        )
        .unwrap();
    let source_http = source.start_http(loopback()).await.unwrap();
    let follower_tcp = follower.start(loopback()).await.unwrap();
    assert_eq!(ticket.peers, vec![format!("tcp://{}", source_tcp.addr())]);

    let joined = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args([
            "--node",
            &format!("tcp://{}", follower_tcp.addr()),
            "join",
            &format!("http://{}/ticket/{}", source_http.addr(), ticket.id),
        ])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stderr)
    );
    assert_eq!(
        follower.replay(ticket.id).unwrap().history,
        source.replay(ticket.id).unwrap().history
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_fallback_still_joins() {
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_tcp = source.start(loopback()).await.unwrap();
    let follower_tcp = follower.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket(
            "Magnet Join",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(30),
        )
        .unwrap();
    assert_eq!(ticket.peers, vec![format!("tcp://{}", source_tcp.addr())]);

    let joined = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args([
            "--node",
            &format!("tcp://{}", follower_tcp.addr()),
            "join",
            &ticket.to_magnet(),
            "--observe",
        ])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stderr)
    );
    let reply: Value = serde_json::from_slice(&joined.stdout).unwrap();
    assert_eq!(reply["id"], ticket.id.to_string());
    assert_eq!(reply["role"], "observe");
    assert_eq!(
        follower.replay(ticket.id).unwrap().history,
        source.replay(ticket.id).unwrap().history
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokened_http_ticket_join_sends_bearer_and_authenticates_swarm() {
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_tcp = source.start(loopback()).await.unwrap();
    let source_http = source.start_http(loopback()).await.unwrap();
    let follower_tcp = follower.start(loopback()).await.unwrap();
    let token = "4242424242424242424242424242424242424242424242424242424242424242";
    let binary = env!("CARGO_BIN_EXE_conch");

    let created = Command::new(binary)
        .args([
            "--node",
            &format!("tcp://{}", source_tcp.addr()),
            "create",
            "--name",
            "Private Room",
            "--token",
            token,
        ])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    let room = created["id"].as_str().unwrap();
    let local_ticket =
        Ticket::from_json_slice(&fs::read(output_dir.path().join("private-room.conch")).unwrap())
            .unwrap();
    assert_eq!(local_ticket.token.unwrap().to_string(), token);

    let joined = Command::new(binary)
        .args([
            "--node",
            &format!("tcp://{}", follower_tcp.addr()),
            "--token",
            token,
            "join",
            &format!("http://{}/ticket/{room}", source_http.addr()),
        ])
        .current_dir(output_dir.path())
        .output()
        .await
        .unwrap();
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stderr)
    );
    assert_eq!(
        follower.replay(local_ticket.id).unwrap().history,
        source.replay(local_ticket.id).unwrap().history
    );
}

#[tokio::test]
async fn current_room_file_supplies_the_cli_default() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "Current Room",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(30),
        )
        .unwrap();
    let server = daemon.start(loopback()).await.unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["--node", &format!("tcp://{}", server.addr()), "status"])
        .env("CONCH_DATA_DIR", data.path())
        .output()
        .await
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["room"], ticket.id.to_string());
}

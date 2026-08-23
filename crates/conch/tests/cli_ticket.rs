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

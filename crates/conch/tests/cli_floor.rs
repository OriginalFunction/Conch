use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    process::Stdio,
    time::Instant,
};

use conchd::tcp::Daemon;
use serde_json::Value;
use tempfile::TempDir;
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    time::{timeout, Duration},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_wait_for_floor_processes_only_unblock_one() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let server = daemon.start(bind).await.unwrap();
    let node = format!("tcp://{}", server.addr());
    let binary = env!("CARGO_BIN_EXE_conch");

    let created = Command::new(binary)
        .args(["--node", &node, "create", "--name", "cli test"])
        .output()
        .await
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let room = serde_json::from_slice::<Value>(&created.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let start = Instant::now();
    let mut alpha = wait_command(binary, &node, &room, "alpha");
    let mut beta = wait_command(binary, &node, &room, "beta");
    let (alpha, beta) = timeout(Duration::from_secs(4), async move {
        tokio::join!(alpha.output(), beta.output())
    })
    .await
    .unwrap();
    let elapsed = start.elapsed();
    let alpha = alpha.unwrap();
    let beta = beta.unwrap();

    assert_ne!(alpha.status.success(), beta.status.success());
    assert!(elapsed >= Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_speak_retry_yield_and_next_waiter() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let server = daemon.start(bind).await.unwrap();
    let mut node = format!("tcp://{}", server.addr());
    let binary = env!("CARGO_BIN_EXE_conch");
    let room = create(binary, &node).await;

    let alpha = wait_command(binary, &node, &room, "alpha")
        .output()
        .await
        .unwrap();
    assert!(alpha.status.success());
    let beta = wait_command(binary, &node, &room, "beta")
        .output()
        .await
        .unwrap();
    assert!(!beta.status.success(), "the live grant keeps beta queued");

    let request_id = "0123456789abcdef0123456789abcdef";
    let first = speak(binary, &node, &room, "alpha", request_id, "hello").await;
    server.abort();
    drop(server);
    drop(daemon);
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(bind).await.unwrap();
    node = format!("tcp://{}", server.addr());
    let retry = speak(binary, &node, &room, "alpha", request_id, "ignored").await;
    assert!(first.status.success());
    assert!(retry.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&first.stdout).unwrap(),
        serde_json::from_slice::<Value>(&retry.stdout).unwrap()
    );

    let yielded = run(
        binary,
        &[
            "--node", &node, "--agent", "alpha", "--room", &room, "yield",
        ],
    )
    .await;
    assert!(
        yielded.status.success(),
        "{}",
        String::from_utf8_lossy(&yielded.stderr)
    );

    let beta = wait_command(binary, &node, &room, "beta")
        .output()
        .await
        .unwrap();
    assert!(
        beta.status.success(),
        "beta receives the next committed grant"
    );

    let late = speak(binary, &node, &room, "alpha", &"ab".repeat(16), "late").await;
    assert!(!late.status.success());
    assert!(String::from_utf8_lossy(&late.stderr).contains("no_grant"));
}

async fn create(binary: &str, node: &str) -> String {
    let created = run(binary, &["--node", node, "create", "--name", "cli test"]).await;
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    serde_json::from_slice::<Value>(&created.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn speak(
    binary: &str,
    node: &str,
    room: &str,
    agent: &str,
    request_id: &str,
    text: &str,
) -> std::process::Output {
    let mut command = Command::new(binary);
    command
        .args([
            "--node",
            node,
            "--agent",
            agent,
            "--room",
            room,
            "speak",
            "--request-id",
            request_id,
            "--file",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .await
        .unwrap();
    child.wait_with_output().await.unwrap()
}

async fn run(binary: &str, arguments: &[&str]) -> std::process::Output {
    Command::new(binary).args(arguments).output().await.unwrap()
}

fn wait_command(binary: &str, node: &str, room: &str, agent: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .args([
            "--node",
            node,
            "--agent",
            agent,
            "--room",
            room,
            "wait-for-floor",
            "--timeout",
            "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

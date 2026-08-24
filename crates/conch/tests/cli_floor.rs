use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
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
        .current_dir(data.path())
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
    let mut alpha = wait_command(binary, &node, &room, "alpha", data.path());
    let mut beta = wait_command(binary, &node, &room, "beta", data.path());
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
    let room = create(binary, &node, data.path()).await;

    let alpha = wait_command(binary, &node, &room, "alpha", data.path())
        .output()
        .await
        .unwrap();
    assert!(alpha.status.success());
    let beta = wait_command(binary, &node, &room, "beta", data.path())
        .output()
        .await
        .unwrap();
    assert!(!beta.status.success(), "the live grant keeps beta queued");

    let request_id = "0123456789abcdef0123456789abcdef";
    let first = speak(
        binary,
        &node,
        &room,
        "alpha",
        request_id,
        "hello",
        data.path(),
    )
    .await;
    server.abort();
    drop(server);
    drop(daemon);
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(bind).await.unwrap();
    node = format!("tcp://{}", server.addr());
    let retry = speak(
        binary,
        &node,
        &room,
        "alpha",
        request_id,
        "ignored",
        data.path(),
    )
    .await;
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
        data.path(),
    )
    .await;
    assert!(
        yielded.status.success(),
        "{}",
        String::from_utf8_lossy(&yielded.stderr)
    );

    let beta = wait_command(binary, &node, &room, "beta", data.path())
        .output()
        .await
        .unwrap();
    assert!(
        beta.status.success(),
        "beta receives the next committed grant"
    );

    let late = speak(
        binary,
        &node,
        &room,
        "alpha",
        &"ab".repeat(16),
        "late",
        data.path(),
    )
    .await;
    assert!(!late.status.success());
    assert!(String::from_utf8_lossy(&late.stderr).contains("no_grant"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raise_retry_and_wait_while_granted_do_not_queue_a_second_turn() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let server = daemon.start(bind).await.unwrap();
    let node = format!("tcp://{}", server.addr());
    let binary = env!("CARGO_BIN_EXE_conch");
    let room = create(binary, &node, data.path()).await;
    let raise_args = [
        "--node",
        &node,
        "--agent",
        "alpha",
        "--room",
        &room,
        "raise-hand",
    ];

    let first = run(binary, &raise_args, data.path()).await;
    let retry = run(binary, &raise_args, data.path()).await;
    assert!(first.status.success());
    assert!(retry.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&first.stdout).unwrap()["intent_id"],
        serde_json::from_slice::<Value>(&retry.stdout).unwrap()["intent_id"]
    );
    let waited = run(
        binary,
        &[
            "--node",
            &node,
            "--agent",
            "alpha",
            "--room",
            &room,
            "wait-for-floor",
        ],
        data.path(),
    )
    .await;
    assert!(
        waited.status.success(),
        "{}",
        String::from_utf8_lossy(&waited.stderr)
    );
    assert!(speak(
        binary,
        &node,
        &room,
        "alpha",
        &"cd".repeat(16),
        "one turn",
        data.path(),
    )
    .await
    .status
    .success());
    assert!(run(
        binary,
        &["--node", &node, "--agent", "alpha", "--room", &room, "yield",],
        data.path(),
    )
    .await
    .status
    .success());
    let history = run(
        binary,
        &[
            "--node", &node, "--agent", "alpha", "--room", &room, "history",
        ],
        data.path(),
    )
    .await;
    assert!(history.status.success());
    let history: Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(history["scenes"].as_array().unwrap().len(), 3);
    assert_eq!(history["scenes"][2]["scene"]["body"]["type"], "speech");
}

async fn create(binary: &str, node: &str, cwd: &Path) -> String {
    let created = run(
        binary,
        &["--node", node, "create", "--name", "cli test"],
        cwd,
    )
    .await;
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
    cwd: &Path,
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
    command.current_dir(cwd);
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

async fn run(binary: &str, arguments: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(binary)
        .args(arguments)
        .current_dir(cwd)
        .output()
        .await
        .unwrap()
}

fn wait_command(binary: &str, node: &str, room: &str, agent: &str, cwd: &Path) -> Command {
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
    command.current_dir(cwd);
    command
}

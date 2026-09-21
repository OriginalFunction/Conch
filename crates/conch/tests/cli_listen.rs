use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
};

use conchd::tcp::Daemon;
use serde_json::Value;
use tempfile::TempDir;
use tokio::{
    process::Command,
    time::{sleep, timeout, Duration},
};

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

async fn run(binary: &str, node: &str, dir: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(binary);
    command.args(["--node", node]).args(args).current_dir(dir);
    command.output().await.unwrap()
}

/// Speak one whole take as `agent`, from raising a hand to yielding.
async fn say(binary: &str, node: &str, dir: &Path, room: &str, agent: &str, text: &str) {
    let spoken = run(
        binary,
        node,
        dir,
        &[
            "--agent",
            agent,
            "--room",
            room,
            "say",
            text,
            "--timeout",
            "10",
            "--json",
        ],
    )
    .await;
    assert!(
        spoken.status.success(),
        "{}",
        String::from_utf8_lossy(&spoken.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listen_wakes_on_a_mention_and_ignores_a_copy() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let node = format!("tcp://{}", server.addr());
    let binary = env!("CARGO_BIN_EXE_conch");

    let created = run(
        binary,
        &node,
        data.path(),
        &["create", "--name", "listening", "--json"],
    )
    .await;
    assert!(created.status.success());
    let room = serde_json::from_slice::<Value>(&created.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // A watcher for agent:claude, started before anything is said.
    let mut watcher = Command::new(binary);
    watcher
        .args([
            "--node",
            &node,
            "--agent",
            "agent:claude",
            "--room",
            &room,
            "listen",
            "--timeout",
            "30",
            "--json",
        ])
        .current_dir(data.path());
    let watching = tokio::spawn(async move { watcher.output().await.unwrap() });
    sleep(Duration::from_millis(400)).await;

    // A take that names someone else is not this agent's business: naming
    // agent:codex first makes agent:claude a copy, which must not wake it.
    say(
        binary,
        &node,
        data.path(),
        &room,
        "human:operator",
        "@codex please take this one, @claude for visibility",
    )
    .await;
    sleep(Duration::from_millis(600)).await;
    assert!(!watching.is_finished(), "a cc must not wake the watcher");

    // A take that names it first is.
    say(
        binary,
        &node,
        data.path(),
        &room,
        "human:operator",
        "@claude what do you make of it?",
    )
    .await;

    let woken = timeout(Duration::from_secs(20), watching)
        .await
        .expect("the watcher wakes on a mention")
        .unwrap();
    assert!(
        woken.status.success(),
        "{}",
        String::from_utf8_lossy(&woken.stderr)
    );
    let event: Value = serde_json::from_slice(&woken.stdout).unwrap();
    assert_eq!(event["kind"], serde_json::json!("mention"));
    assert_eq!(
        event["text"],
        serde_json::json!("@claude what do you make of it?")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listen_exits_two_when_the_room_stays_quiet() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let node = format!("tcp://{}", server.addr());
    let binary = env!("CARGO_BIN_EXE_conch");

    let created = run(
        binary,
        &node,
        data.path(),
        &["create", "--name", "quiet", "--json"],
    )
    .await;
    let room = serde_json::from_slice::<Value>(&created.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let quiet = run(
        binary,
        &node,
        data.path(),
        &[
            "--agent",
            "agent:claude",
            "--room",
            &room,
            "listen",
            "--timeout",
            "1",
            "--json",
        ],
    )
    .await;
    assert_eq!(
        quiet.status.code(),
        Some(2),
        "a quiet room times out with its own exit code"
    );
    let reported: Value = serde_json::from_slice(&quiet.stdout).unwrap();
    assert_eq!(reported["woke"], serde_json::json!(false));
}

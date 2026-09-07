use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
};

use conchd::tcp::Daemon;
use serde_json::Value;
use tempfile::TempDir;
use tokio::process::Command;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

async fn conch(node: &str, cwd: &Path, data: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["--node", node])
        .args(args)
        .current_dir(cwd)
        .env("CONCH_DATA_DIR", data)
        .env("USER", "Ray.Hwang")
        .env_remove("CONCH_AGENT")
        .env_remove("CONCH_ROOM")
        .output()
        .await
        .unwrap()
}

fn text(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

#[tokio::test]
async fn readable_by_default_and_json_on_request() {
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let node = format!("tcp://{}", server.addr());

    let created = text(
        &conch(
            &node,
            cwd.path(),
            data.path(),
            &["create", "--name", "Design room"],
        )
        .await,
    );
    assert!(
        created.starts_with("created \"Design room\" ("),
        "{created}"
    );
    assert!(
        created.contains("\nticket: ./design-room.conch\nmagnet: conch:1:"),
        "{created}"
    );

    let json = text(&conch(&node, cwd.path(), data.path(), &["status", "--json"]).await);
    let status: Value = serde_json::from_str(&json).expect("--json prints JSON");
    assert_eq!(status["name"], "Design room");
    let readable = text(&conch(&node, cwd.path(), data.path(), &["status"]).await);
    assert!(readable.starts_with("Design room ("), "{readable}");
    assert!(
        readable.contains("\nfloor: vacant\nqueue: empty\n"),
        "{readable}"
    );

    // A turn as the default human identity, then readable history.
    text(
        &conch(
            &node,
            cwd.path(),
            data.path(),
            &["wait-for-floor", "--timeout", "5"],
        )
        .await,
    );
    let mut spoke = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["--node", &node, "speak", "--file", "-"])
        .current_dir(cwd.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("USER", "Ray.Hwang")
        .env_remove("CONCH_AGENT")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use tokio::io::AsyncWriteExt;
        spoke
            .stdin
            .take()
            .unwrap()
            .write_all(b"hello from a person\nsecond line")
            .await
            .unwrap();
    }
    let spoke = spoke.wait_with_output().await.unwrap();
    assert_eq!(text(&spoke), "appended (rev 1)");
    let yielded = text(&conch(&node, cwd.path(), data.path(), &["yield"]).await);
    assert!(
        yielded.starts_with("take frozen (rev 1); closes grant "),
        "{yielded}"
    );

    let history = text(&conch(&node, cwd.path(), data.path(), &["history"]).await);
    assert!(
        history.contains("#1    grant          → human:ray-hwang"),
        "{history}"
    );
    assert!(
        history.contains(
            "#2    human:ray-hwang hello from a person\n                      second line"
        ),
        "{history}"
    );
    let oneline = text(&conch(&node, cwd.path(), data.path(), &["history", "--oneline"]).await);
    assert!(
        oneline.contains("human:ray-hwang hello from a person") && !oneline.contains("second line"),
        "{oneline}"
    );
    server.abort();
}

#[tokio::test]
async fn rooms_lists_and_use_switches_the_current_room() {
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let node = format!("tcp://{}", server.addr());

    assert_eq!(
        text(&conch(&node, cwd.path(), data.path(), &["rooms"]).await),
        "no rooms; conch create --name …"
    );
    let first = text(
        &conch(
            &node,
            cwd.path(),
            data.path(),
            &["create", "--name", "First", "--json"],
        )
        .await,
    );
    let first: Value = serde_json::from_str(&first).unwrap();
    let second = text(
        &conch(
            &node,
            cwd.path(),
            data.path(),
            &["create", "--name", "Second", "--json"],
        )
        .await,
    );
    let second: Value = serde_json::from_str(&second).unwrap();
    let (first_id, second_id) = (
        first["id"].as_str().unwrap(),
        second["id"].as_str().unwrap(),
    );

    // create sets current-room to the newest room, so Second is marked.
    let rooms = text(&conch(&node, cwd.path(), data.path(), &["rooms"]).await);
    assert!(
        rooms.contains(&format!("* {}…  Second", &second_id[..8])),
        "{rooms}"
    );
    assert!(
        rooms.contains(&format!("  {}…  First", &first_id[..8])),
        "{rooms}"
    );
    assert!(rooms.contains("head 0   vacant"), "{rooms}");
    let json = text(&conch(&node, cwd.path(), data.path(), &["rooms", "--json"]).await);
    let summaries: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(summaries["rooms"].as_array().unwrap().len(), 2);

    // use by unique prefix, then by exact name.
    assert_eq!(
        text(&conch(&node, cwd.path(), data.path(), &["use", &first_id[..6]]).await),
        format!("using \"First\" ({}…)", &first_id[..8])
    );
    assert_eq!(
        std::fs::read_to_string(data.path().join("current-room")).unwrap(),
        format!("\"{first_id}\"")
    );
    assert!(
        text(&conch(&node, cwd.path(), data.path(), &["rooms"]).await)
            .contains(&format!("* {}…  First", &first_id[..8]))
    );
    assert_eq!(
        text(&conch(&node, cwd.path(), data.path(), &["use", "Second", "--json"]).await),
        format!("{{\"id\":\"{second_id}\",\"name\":\"Second\"}}")
    );

    // Unknown input fails and names what was asked for; the ambiguous-prefix
    // case is covered deterministically by `resolve_room_query`'s unit tests
    // in `crates/conch/src/main.rs` (two random room ids rarely share a
    // leading nibble, so this subprocess test can't rely on it).
    let unknown = conch(&node, cwd.path(), data.path(), &["use", "Nowhere"]).await;
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("no room matches \"Nowhere\""));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn say_takes_one_turn_and_reports_the_committed_scene() {
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let node = format!("tcp://{}", server.addr());
    text(
        &conch(
            &node,
            cwd.path(),
            data.path(),
            &["create", "--name", "Chat"],
        )
        .await,
    );

    assert_eq!(
        text(&conch(&node, cwd.path(), data.path(), &["say", "hello everyone"]).await),
        "said #2 as human:ray-hwang"
    );
    let json = text(
        &conch(
            &node,
            cwd.path(),
            data.path(),
            &["say", "again", "--json", "--agent", "agent:codex"],
        )
        .await,
    );
    let said: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(said["n"], 4);
    assert_eq!(said["author"]["agent"], "agent:codex");
    assert_eq!(said["grant_hash"].as_str().unwrap().len(), 64);
    let history = text(&conch(&node, cwd.path(), data.path(), &["history"]).await);
    assert!(
        history.contains("#2    human:ray-hwang hello everyone"),
        "{history}"
    );
    assert!(history.contains("#4    agent:codex    again"), "{history}");

    let empty = conch(&node, cwd.path(), data.path(), &["say", ""]).await;
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("say needs text"));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn say_reports_a_floor_timeout_with_the_queue_position() {
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let node = format!("tcp://{}", server.addr());
    text(
        &conch(
            &node,
            cwd.path(),
            data.path(),
            &["create", "--name", "Busy"],
        )
        .await,
    );
    // agent:holder takes the floor and keeps it.
    text(
        &conch(
            &node,
            cwd.path(),
            data.path(),
            &[
                "wait-for-floor",
                "--timeout",
                "5",
                "--agent",
                "agent:holder",
            ],
        )
        .await,
    );

    let late = conch(
        &node,
        cwd.path(),
        data.path(),
        &["say", "me next", "--timeout", "1"],
    )
    .await;
    assert!(!late.status.success());
    let err = String::from_utf8_lossy(&late.stderr);
    assert!(
        err.contains("timeout: no floor within 1 s (queue position 1)"),
        "{err}"
    );
    server.abort();
}

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

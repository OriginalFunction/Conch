use std::{fs, net::TcpListener, path::PathBuf, process::Command, time::Duration};

use conch_launch::PidFile;
use tempfile::TempDir;

/// conchd built by the workspace; fall back to building it so `cargo test -p conch` works alone.
fn conchd_binary() -> PathBuf {
    let sibling = PathBuf::from(env!("CARGO_BIN_EXE_conch")).with_file_name("conchd");
    if !sibling.is_file() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "conchd", "--bin", "conchd"])
            .status()
            .unwrap();
        assert!(status.success());
    }
    sibling
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn conch(data: &TempDir, tcp: u16, http: u16, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(args)
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", format!("127.0.0.1:{tcp}"))
        .env("CONCH_DEFAULT_HTTP", format!("127.0.0.1:{http}"))
        .env_remove("CONCH_NODE")
        .output()
        .unwrap()
}

#[test]
fn up_spawns_and_down_stops() {
    let data = TempDir::new().unwrap();
    let (tcp, http) = (free_port(), free_port());
    let up = conch(&data, tcp, http, &["up"]);
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stderr)
    );
    let out = String::from_utf8_lossy(&up.stdout);
    assert!(out.contains(&format!("http://127.0.0.1:{http}/")), "{out}");
    let pid = PidFile::read(data.path()).unwrap();
    assert!(pid.is_alive());
    let again = conch(&data, tcp, http, &["up"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already running"));
    let down = conch(&data, tcp, http, &["down"]);
    assert!(
        down.status.success(),
        "{}",
        String::from_utf8_lossy(&down.stderr)
    );
    assert!(PidFile::read(data.path()).is_none());
}

#[test]
fn status_auto_spawns_on_default_node_and_says_so() {
    let data = TempDir::new().unwrap();
    let (tcp, http) = (free_port(), free_port());
    let status = conch(&data, tcp, http, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(String::from_utf8_lossy(&status.stderr).contains("conch: started conchd (pid "));
    assert!(String::from_utf8_lossy(&status.stdout).contains("\"rooms\""));
    PidFile::read(data.path())
        .unwrap()
        .stop(Duration::from_secs(5))
        .unwrap();
}

#[test]
fn explicit_node_never_spawns_and_prints_remedy() {
    let data = TempDir::new().unwrap();
    let dead = free_port();
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["--node", &format!("tcp://127.0.0.1:{dead}"), "status"])
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains(&format!("conchd is not running on 127.0.0.1:{dead}")),
        "{err}"
    );
    assert!(err.contains("`conch up`"));
    assert!(PidFile::read(data.path()).is_none());
    assert!(!fs::exists(data.path().join("conchd.log")).unwrap_or(false));
}

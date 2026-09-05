use std::{fs, net::TcpListener, path::PathBuf, process::Command, time::Duration};

use conch_launch::PidFile;
use tempfile::TempDir;

fn conchd_binary() -> PathBuf {
    let sibling = PathBuf::from(env!("CARGO_BIN_EXE_conch")).with_file_name("conchd");
    if !sibling.is_file() {
        assert!(Command::new(env!("CARGO"))
            .args(["build", "-p", "conchd", "--bin", "conchd"])
            .status()
            .unwrap()
            .success());
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

fn doctor(home: &TempDir, data: &TempDir, tcp: u16) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("doctor")
        .env("HOME", home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", format!("127.0.0.1:{tcp}"))
        .env("CONCH_DEFAULT_HTTP", format!("127.0.0.1:{}", free_port()))
        .env_remove("CONCH_NODE")
        .output()
        .unwrap()
}

#[test]
fn doctor_fails_without_a_daemon_and_names_the_remedy() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let output = doctor(&home, &data, free_port());
    assert_eq!(output.status.code(), Some(1));
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("fail  daemon"), "{out}");
    assert!(out.contains("conch up"), "{out}");
}

#[test]
fn doctor_passes_with_daemon_and_reports_hosts() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let tcp = free_port();
    // configure one host and start a daemon
    assert!(Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["setup", "cursor"])
        .env("HOME", home.path())
        .env("CONCH_SETUP_SKIP_DAEMON", "1")
        .status()
        .unwrap()
        .success());
    assert!(Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("up")
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", format!("127.0.0.1:{tcp}"))
        .env("CONCH_DEFAULT_HTTP", format!("127.0.0.1:{}", free_port()))
        .env_remove("CONCH_NODE")
        .status()
        .unwrap()
        .success());
    let output = doctor(&home, &data, tcp);
    let out = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "ok    daemon        reachable on 127.0.0.1:{tcp}, version {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{out}"
    );
    assert!(out.contains("warn  daemon        started by hand"), "{out}");
    assert!(out.contains("ok    cursor        agent:cursor"), "{out}");
    assert!(out.contains("--    claude        not configured"), "{out}");
    PidFile::read(data.path())
        .unwrap()
        .stop(Duration::from_secs(5))
        .unwrap();
}

#[test]
fn doctor_flags_a_stale_skill() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    assert!(Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["setup", "claude"])
        .env("HOME", home.path())
        .env("CONCH_SETUP_SKIP_DAEMON", "1")
        .status()
        .unwrap()
        .success());
    let skill = home.path().join(".claude/skills/join-room/SKILL.md");
    fs::write(
        &skill,
        fs::read_to_string(&skill)
            .unwrap()
            .replace(env!("CARGO_PKG_VERSION"), "0.0.1"),
    )
    .unwrap();
    let out = String::from_utf8_lossy(&doctor(&home, &data, free_port()).stdout).into_owned();
    assert!(
        out.contains("warn  claude        agent:claude, skill 0.0.1 is stale"),
        "{out}"
    );
}

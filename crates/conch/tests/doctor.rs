use std::{
    fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

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

/// Two loopback ports that stay reserved until the caller drops the listeners, closing
/// the window in which another test (or anything else on the machine) could take them.
fn reserve_ports() -> (SocketAddr, SocketAddr, (TcpListener, TcpListener)) {
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    let addrs = (tcp.local_addr().unwrap(), http.local_addr().unwrap());
    (addrs.0, addrs.1, (tcp, http))
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

fn doctor(home: &TempDir, data: &TempDir, tcp: SocketAddr) -> std::process::Output {
    let (_unused, http, reserved) = reserve_ports();
    drop(reserved);
    Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("doctor")
        .env("HOME", home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", tcp.to_string())
        .env("CONCH_DEFAULT_HTTP", http.to_string())
        .env_remove("CONCH_NODE")
        .env_remove("CODEX_HOME")
        .env_remove("GROK_HOME")
        .env_remove("OPENCODE_CONFIG")
        .output()
        .unwrap()
}

fn setup(home: &TempDir, host: &str) {
    assert!(Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["setup", host])
        .env("HOME", home.path())
        .env("CONCH_SETUP_SKIP_DAEMON", "1")
        .env_remove("CODEX_HOME")
        .env_remove("GROK_HOME")
        .env_remove("OPENCODE_CONFIG")
        .status()
        .unwrap()
        .success());
}

#[test]
fn doctor_fails_without_a_daemon_and_names_the_remedy() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let (dead, _http, reserved) = reserve_ports();
    drop(reserved);
    let output = doctor(&home, &data, dead);
    assert_eq!(output.status.code(), Some(1));
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("fail  daemon"), "{out}");
    assert!(out.contains("conch up"), "{out}");
}

#[test]
fn doctor_passes_with_daemon_and_reports_hosts() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let _guard = DaemonGuard::new(data.path());
    let (tcp, http, reserved) = reserve_ports();
    // configure one host and start a daemon
    setup(&home, "cursor");
    drop(reserved);
    assert!(Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("up")
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", tcp.to_string())
        .env("CONCH_DEFAULT_HTTP", http.to_string())
        .env_remove("CONCH_NODE")
        .status()
        .unwrap()
        .success());
    let output = doctor(&home, &data, tcp);
    let out = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "ok    daemon        reachable on {tcp}, version {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{out}"
    );
    assert!(out.contains("warn  daemon        started by hand"), "{out}");
    assert!(out.contains("ok    cursor        agent:cursor"), "{out}");
    assert!(out.contains("--    claude        not configured"), "{out}");
}

#[test]
fn doctor_flags_a_recorded_command_that_no_longer_exists() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    setup(&home, "cursor");
    // What a Homebrew upgrade leaves behind: an entry pointing at a versioned path
    // that has since been deleted.
    let config = home.path().join(".cursor/mcp.json");
    let rewritten = fs::read_to_string(&config).unwrap().replace(
        env!("CARGO_BIN_EXE_conch"),
        "/nonexistent/Cellar/conch/bin/conch",
    );
    fs::write(&config, rewritten).unwrap();

    let (dead, _http, reserved) = reserve_ports();
    drop(reserved);
    let out = String::from_utf8_lossy(&doctor(&home, &data, dead).stdout).into_owned();
    assert!(
        out.contains(
            "warn  cursor        agent:cursor, command /nonexistent/Cellar/conch/bin/conch missing"
        ),
        "{out}"
    );
    assert!(out.contains("run `conch setup cursor`"), "{out}");
}

#[test]
fn doctor_flags_a_stale_skill() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    setup(&home, "claude");
    let skill = home.path().join(".claude/skills/join-room/SKILL.md");
    fs::write(
        &skill,
        fs::read_to_string(&skill)
            .unwrap()
            .replace(env!("CARGO_PKG_VERSION"), "0.0.1"),
    )
    .unwrap();
    let (dead, _http, reserved) = reserve_ports();
    drop(reserved);
    let out = String::from_utf8_lossy(&doctor(&home, &data, dead).stdout).into_owned();
    assert!(
        out.contains("warn  claude        agent:claude, skill 0.0.1 is stale"),
        "{out}"
    );
}

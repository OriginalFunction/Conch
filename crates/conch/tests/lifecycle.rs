use std::{
    fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

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

fn conch(data: &TempDir, tcp: SocketAddr, http: SocketAddr, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(args)
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", tcp.to_string())
        .env("CONCH_DEFAULT_HTTP", http.to_string())
        .env_remove("CONCH_NODE")
        .output()
        .unwrap()
}

#[test]
fn up_spawns_and_down_stops() {
    let data = TempDir::new().unwrap();
    let _guard = DaemonGuard::new(data.path());
    let (tcp, http, reserved) = reserve_ports();
    drop(reserved);
    let up = conch(&data, tcp, http, &["up"]);
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stderr)
    );
    let out = String::from_utf8_lossy(&up.stdout);
    assert!(out.contains(&format!("http://{http}/")), "{out}");
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
    let _guard = DaemonGuard::new(data.path());
    let (tcp, http, reserved) = reserve_ports();
    drop(reserved);
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
    let (dead, _http, reserved) = reserve_ports();
    drop(reserved);
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["--node", &format!("tcp://{dead}"), "status"])
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains(&format!("conchd is not running on {dead}")),
        "{err}"
    );
    assert!(err.contains("`conch up`"));
    assert!(PidFile::read(data.path()).is_none());
    assert!(!fs::exists(data.path().join("conchd.log")).unwrap_or(false));
}

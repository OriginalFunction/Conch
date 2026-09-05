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

/// A fake `launchctl`/`systemctl` that records its argv and, for the "start it now"
/// verb, launches conchd itself — standing in for the service manager without ever
/// touching the real one. Returns (stub directory, argv log, pid the stub started).
fn service_manager_stub(
    data: &TempDir,
    tcp: SocketAddr,
    http: SocketAddr,
) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().unwrap();
    let record = dir.path().join("argv.log");
    let started_pid = dir.path().join("started.pid");
    let name = if cfg!(target_os = "macos") {
        "launchctl"
    } else {
        "systemctl"
    };
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> '{record}'\n\
         case \"$*\" in\n\
         \x20 *bootstrap*|*'enable --now'*)\n\
         \x20   '{conchd}' --localhost --data-dir '{data}' --tcp {tcp} --http {http} \
         </dev/null >> '{log}' 2>&1 &\n\
         \x20   printf '%s\\n' \"$!\" > '{started_pid}'\n\
         \x20   ;;\n\
         esac\n\
         exit 0\n",
        record = record.display(),
        conchd = conchd_binary().display(),
        data = data.path().display(),
        tcp = tcp,
        http = http,
        log = data.path().join("conchd.log").display(),
        started_pid = started_pid.display(),
    );
    let path = dir.path().join(name);
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, record, started_pid)
}

fn conchd_processes_for(data: &TempDir) -> usize {
    let output = Command::new("pgrep")
        .arg("-f")
        .arg(data.path().display().to_string())
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

#[test]
fn up_service_installs_unit_without_hand_spawn() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let _guard = DaemonGuard::new(data.path());
    let (tcp, http, reserved) = reserve_ports();
    let (stub, record, started_pid) = service_manager_stub(&data, tcp, http);
    drop(reserved);

    let up = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["up", "--service"])
        .env("HOME", home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", tcp.to_string())
        .env("CONCH_DEFAULT_HTTP", http.to_string())
        .env(
            "PATH",
            format!(
                "{}:{}",
                stub.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env_remove("CONCH_NODE")
        .output()
        .unwrap();
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stderr)
    );

    // The unit was written under the temp HOME and handed to the service manager.
    let unit = if cfg!(target_os = "macos") {
        home.path()
            .join("Library/LaunchAgents/com.conch.conchd.plist")
    } else {
        home.path().join(".config/systemd/user/conchd.service")
    };
    assert!(unit.is_file(), "unit not written to {}", unit.display());
    let calls = fs::read_to_string(&record).unwrap();
    assert!(
        calls.contains("bootstrap") || calls.contains("enable --now"),
        "{calls}"
    );

    // Exactly one conchd is running, and it is the one the service manager started —
    // `up --service` must not hand-spawn a daemon the unit would then fight with.
    let by_service: u32 = fs::read_to_string(&started_pid)
        .expect("stub started a daemon")
        .trim()
        .parse()
        .unwrap();
    let pid = PidFile::read(data.path()).expect("pid file from the service-started daemon");
    assert_eq!(pid.pid, by_service, "the daemon was hand-spawned as well");
    assert!(pid.is_alive());
    assert_eq!(conchd_processes_for(&data), 1);

    let out = String::from_utf8_lossy(&up.stdout);
    assert!(out.contains(&format!("http://{http}/")), "{out}");
    assert!(out.contains(&format!("pid {by_service}")), "{out}");
}

#[test]
fn down_never_signals_a_recycled_pid_and_clears_the_stale_file() {
    let data = TempDir::new().unwrap();
    let (tcp, http, reserved) = reserve_ports();
    drop(reserved);
    // A pid file left by a crashed daemon whose pid has since been recycled by an
    // unrelated process — here, the test harness itself.
    PidFile {
        pid: std::process::id(),
        tcp,
        http,
    }
    .write(data.path())
    .unwrap();
    let down = conch(&data, tcp, http, &["down"]);
    assert!(
        down.status.success(),
        "{}",
        String::from_utf8_lossy(&down.stderr)
    );
    let out = String::from_utf8_lossy(&down.stdout);
    assert!(out.contains("conchd is not running"), "{out}");
    assert!(
        PidFile::read(data.path()).is_none(),
        "stale pid file removed"
    );
    // The unrelated process is untouched.
    assert!(PidFile {
        pid: std::process::id(),
        tcp,
        http
    }
    .is_alive());
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
fn default_node_spawn_failure_prints_remedy() {
    let data = TempDir::new().unwrap();
    let (dead, http, reserved) = reserve_ports();
    drop(reserved);
    // Run a copy of the CLI from a directory with no conchd beside it, with no
    // override and no PATH, so locating conchd cannot succeed.
    let bin_dir = TempDir::new().unwrap();
    let conch_copy = bin_dir.path().join("conch");
    fs::copy(env!("CARGO_BIN_EXE_conch"), &conch_copy).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&conch_copy, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let output = Command::new(&conch_copy)
        .arg("status")
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", "/nonexistent/conchd")
        .env("CONCH_DEFAULT_TCP", dead.to_string())
        .env("CONCH_DEFAULT_HTTP", http.to_string())
        .env_remove("CONCH_NODE")
        .env_remove("PATH")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains(&format!("conchd is not running on {dead}")),
        "{err}"
    );
    assert!(err.contains("`conch up`"), "{err}");
    assert!(err.contains("conchd binary not found"), "{err}");
    assert!(PidFile::read(data.path()).is_none());
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

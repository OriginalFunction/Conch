use std::{
    fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
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

/// Two loopback ports for a daemon this test is about to start.
///
/// They come from below the ephemeral range (macOS hands out 49152 and up), because an
/// ephemeral port is offered to unrelated processes the instant the reservation is
/// released — and it has to be released before conchd can bind it. The cursor only ever
/// walks forward, so no port is issued twice in this binary; its starting band is
/// derived from both the clock and the pid, so binaries running side by side do not
/// overlap and a port is not offered again for minutes. The listeners stay bound until
/// the caller drops them, immediately before the daemon runs.
fn reserve_ports() -> (SocketAddr, SocketAddr, (TcpListener, TcpListener)) {
    static NEXT: Mutex<Option<u16>> = Mutex::new(None);
    let mut next = NEXT.lock().expect("port cursor is not poisoned");
    let cursor = next.get_or_insert_with(|| {
        // The band advances with the clock as well as the pid, so a port is not offered
        // again for several minutes. A daemon that has just been stopped can leave a
        // connection endpoint on its listening port for tens of seconds, and that
        // outlives a scheme that only cycles through a few bands.
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_secs();
        20_000 + ((seconds + std::process::id() as u64) % 600) as u16 * 30
    });
    let mut chosen = Vec::new();
    while chosen.len() < 2 {
        let candidate = *cursor;
        *cursor = if candidate >= 38_999 {
            20_000
        } else {
            candidate + 1
        };
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", candidate)) {
            chosen.push(listener);
        }
    }
    let http = chosen.pop().expect("two ports");
    let tcp = chosen.pop().expect("two ports");
    let addrs = (tcp.local_addr().unwrap(), http.local_addr().unwrap());
    (addrs.0, addrs.1, (tcp, http))
}

/// An address whose connection is always refused: nothing may bind a port below 1024
/// without root, and nothing listens on port 1. Tests that need "no daemon here" use
/// this rather than a released ephemeral port, which anything on the machine may take.
fn refused_addr() -> SocketAddr {
    "127.0.0.1:1".parse().expect("literal address")
}

/// Reserve a fresh pair of ports and hand them to `start`, retrying when the daemon
/// could not bind one of them.
///
/// The reservation has to be released before conchd can take the port, and in that
/// window anything on the machine may claim it — the daemon then exits with "address
/// already in use". That is a property of the harness, not of the code under test, so
/// the test picks another pair rather than failing.
fn with_daemon_ports(
    mut start: impl FnMut(SocketAddr, SocketAddr) -> bool,
) -> (SocketAddr, SocketAddr) {
    for attempt in 1..=4 {
        let (tcp, http, reserved) = reserve_ports();
        drop(reserved);
        if start(tcp, http) {
            return (tcp, http);
        }
        assert!(attempt < 4, "conchd could not bind any reserved port pair");
    }
    unreachable!("the loop either returns or asserts")
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
    // `doctor` never connects to the HTTP address; it only has to be set.
    let http = refused_addr();
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
fn doctor_reports_a_broken_environment_instead_of_bailing_out() {
    let data = TempDir::new().unwrap();
    // doctor exists to diagnose a broken install; an unset HOME or a mistyped
    // CONCH_DEFAULT_TCP must appear as checks, not abort the whole report.
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("doctor")
        .env_remove("HOME")
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_DEFAULT_TCP", "not-an-address")
        .env_remove("CONCH_NODE")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("fail  daemon"), "{out}");
    assert!(out.contains("not-an-address"), "{out}");
    assert!(out.contains("fail  hosts"), "{out}");
    assert!(out.contains("HOME is not set"), "{out}");
    // and it still printed the checks that do not depend on either
    assert!(out.contains("data dir"), "{out}");
}

#[test]
fn doctor_names_a_duplicate_install_once_when_path_repeats_a_directory() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let other = TempDir::new().unwrap();
    fs::write(other.path().join("conch"), "#!/bin/sh\n").unwrap();
    let dead = refused_addr();
    let repeated = format!("{0}:{0}", other.path().display());
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("doctor")
        .env("HOME", home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_DEFAULT_TCP", dead.to_string())
        .env("PATH", repeated)
        .env_remove("CONCH_NODE")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    let duplicate = other.path().join("conch").display().to_string();
    assert!(out.contains(&duplicate), "{out}");
    assert_eq!(
        out.matches(&duplicate).count(),
        1,
        "duplicate listed once per install, not once per PATH entry: {out}"
    );
}

#[test]
fn doctor_distinguishes_an_unversioned_skill_from_a_missing_one() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    setup(&home, "claude");
    let skill = home.path().join(".claude/skills/join-room/SKILL.md");
    // A copy written before skills carried a version marker.
    fs::write(&skill, "---\nname: join-room\n---\n\n# Join a room\n").unwrap();
    let dead = refused_addr();
    let out = String::from_utf8_lossy(&doctor(&home, &data, dead).stdout).into_owned();
    assert!(
        out.contains("warn  claude        agent:claude, skill unversioned (pre-1.3)"),
        "{out}"
    );
}

#[test]
fn doctor_fails_without_a_daemon_and_names_the_remedy() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let dead = refused_addr();
    let output = doctor(&home, &data, dead);
    let out = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{dead}\n{out}");
    assert!(out.contains("fail  daemon"), "{out}");
    assert!(out.contains("conch up"), "{out}");
}

#[test]
fn doctor_passes_with_daemon_and_reports_hosts() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let _guard = DaemonGuard::new(data.path());
    // configure one host and start a daemon
    setup(&home, "cursor");
    let (tcp, _http) = with_daemon_ports(|tcp, http| {
        Command::new(env!("CARGO_BIN_EXE_conch"))
            .arg("up")
            .env("CONCH_DATA_DIR", data.path())
            .env("CONCH_CONCHD", conchd_binary())
            .env("CONCH_DEFAULT_TCP", tcp.to_string())
            .env("CONCH_DEFAULT_HTTP", http.to_string())
            .env_remove("CONCH_NODE")
            .status()
            .unwrap()
            .success()
    });
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

    let dead = refused_addr();
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
    let dead = refused_addr();
    let out = String::from_utf8_lossy(&doctor(&home, &data, dead).stdout).into_owned();
    assert!(
        out.contains("warn  claude        agent:claude, skill 0.0.1 is stale"),
        "{out}"
    );
}

/// Rewrite cursor's recorded command to the bare name `conch`, as a hand-written
/// entry or a `claude mcp add conch` would leave it.
fn record_bare_command(home: &TempDir) {
    let config = home.path().join(".cursor/mcp.json");
    let rewritten = fs::read_to_string(&config)
        .unwrap()
        .replace(env!("CARGO_BIN_EXE_conch"), "conch");
    fs::write(&config, rewritten).unwrap();
}

#[test]
fn doctor_resolves_a_bare_command_name_through_path() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    setup(&home, "cursor");
    record_bare_command(&home);
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_conch")).parent().unwrap();
    let dead = refused_addr();
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("doctor")
        .env("HOME", home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_DEFAULT_TCP", dead.to_string())
        .env("PATH", bin_dir)
        .env_remove("CONCH_NODE")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("ok    cursor        agent:cursor"), "{out}");
}

#[test]
fn doctor_flags_a_bare_command_name_that_is_not_on_path() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    setup(&home, "cursor");
    record_bare_command(&home);
    let empty = TempDir::new().unwrap();
    let dead = refused_addr();
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("doctor")
        .env("HOME", home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_DEFAULT_TCP", dead.to_string())
        .env("PATH", empty.path())
        .env_remove("CONCH_NODE")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(
        out.contains("warn  cursor        agent:cursor, command conch missing"),
        "{out}"
    );
}

/// A fake `launchctl`/`systemctl` whose only job is to answer "is the unit loaded?"
/// with a fixed exit code. Returns the directory to put on PATH.
fn service_manager_answering(loaded: bool) -> TempDir {
    let dir = TempDir::new().unwrap();
    let name = if cfg!(target_os = "macos") {
        "launchctl"
    } else {
        "systemctl"
    };
    let path = dir.path().join(name);
    fs::write(&path, format!("#!/bin/sh\nexit {}\n", u8::from(!loaded))).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn write_unit_file(home: &TempDir) {
    let unit = if cfg!(target_os = "macos") {
        home.path()
            .join("Library/LaunchAgents/com.conch.conchd.plist")
    } else {
        home.path().join(".config/systemd/user/conchd.service")
    };
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, "placeholder unit\n").unwrap();
}

fn doctor_with_service_manager(
    home: &TempDir,
    data: &TempDir,
    tcp: SocketAddr,
    manager: &TempDir,
) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("doctor")
        .env("HOME", home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", tcp.to_string())
        .env("CONCH_DEFAULT_HTTP", refused_addr().to_string())
        .env(
            "PATH",
            format!(
                "{}:{}",
                manager.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env_remove("CONCH_NODE")
        .env_remove("CODEX_HOME")
        .env_remove("GROK_HOME")
        .env_remove("OPENCODE_CONFIG")
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn doctor_tells_a_loaded_unit_from_one_that_is_merely_present() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let _guard = DaemonGuard::new(data.path());
    write_unit_file(&home);
    let (tcp, _http) = with_daemon_ports(|tcp, http| {
        Command::new(env!("CARGO_BIN_EXE_conch"))
            .arg("up")
            .env("CONCH_DATA_DIR", data.path())
            .env("CONCH_CONCHD", conchd_binary())
            .env("CONCH_DEFAULT_TCP", tcp.to_string())
            .env("CONCH_DEFAULT_HTTP", http.to_string())
            .env_remove("CONCH_NODE")
            .status()
            .unwrap()
            .success()
    });

    let out = doctor_with_service_manager(&home, &data, tcp, &service_manager_answering(true));
    assert!(
        out.contains("ok    daemon        service unit loaded"),
        "{out}"
    );

    // The file is there but the service manager does not know it: a reboot will
    // not bring conchd back, which is the whole point of the unit.
    let out = doctor_with_service_manager(&home, &data, tcp, &service_manager_answering(false));
    assert!(
        out.contains("warn  daemon        service unit present but not loaded"),
        "{out}"
    );
    assert!(out.contains("run `conch up --service`"), "{out}");
}

#[test]
fn doctor_reports_free_space_and_the_current_rooms_name_and_head() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let _guard = DaemonGuard::new(data.path());
    let (tcp, _http) = with_daemon_ports(|tcp, http| {
        Command::new(env!("CARGO_BIN_EXE_conch"))
            .arg("up")
            .env("CONCH_DATA_DIR", data.path())
            .env("CONCH_CONCHD", conchd_binary())
            .env("CONCH_DEFAULT_TCP", tcp.to_string())
            .env("CONCH_DEFAULT_HTTP", http.to_string())
            .env_remove("CONCH_NODE")
            .status()
            .unwrap()
            .success()
    });
    // `create` records the new room as current.
    let created = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["create", "--name", "Doctor Room"])
        .current_dir(home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_DEFAULT_TCP", tcp.to_string())
        .env_remove("CONCH_NODE")
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let out = String::from_utf8_lossy(&doctor(&home, &data, tcp).stdout).into_owned();
    let room_line = out
        .lines()
        .find(|line| line.contains("current room"))
        .unwrap_or_else(|| panic!("no current room line in:\n{out}"));
    assert!(room_line.starts_with("ok    current room"), "{room_line}");
    assert!(room_line.contains("\"Doctor Room\""), "{room_line}");
    assert!(room_line.contains("head 0"), "{room_line}");
    let data_line = out
        .lines()
        .find(|line| line.contains("data dir"))
        .unwrap_or_else(|| panic!("no data dir line in:\n{out}"));
    assert!(data_line.contains(" free)"), "{data_line}");
}

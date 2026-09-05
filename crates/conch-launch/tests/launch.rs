use std::{
    ffi::OsString,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    time::Duration,
};

use conch_launch::{locate_conchd_in, wait_for_port, PidFile};
use tempfile::TempDir;

#[test]
fn locate_prefers_env_override_and_reports_search_locations() {
    let dir = TempDir::new().unwrap();
    let fake = dir.path().join("conchd");
    fs::write(&fake, "#!/bin/sh\n").unwrap();
    let fake_os = OsString::from(&fake);

    // The explicit override wins over everything else.
    assert_eq!(locate_conchd_in(Some(&fake_os), None, None).unwrap(), fake);

    // A missing override falls through to the directory beside the running binary.
    let missing = OsString::from("/definitely/not/here/conchd");
    assert_eq!(
        locate_conchd_in(Some(&missing), Some(dir.path()), None).unwrap(),
        fake
    );

    // With nothing found, every location tried is named, including each PATH entry.
    let empty = TempDir::new().unwrap();
    let path = std::env::join_paths([empty.path()]).unwrap();
    let error = locate_conchd_in(Some(&missing), Some(empty.path()), Some(&path))
        .unwrap_err()
        .to_string();
    assert!(error.contains("/definitely/not/here/conchd"), "{error}");
    assert!(
        error.contains(&empty.path().join("conchd").display().to_string()),
        "{error}"
    );
    assert!(!error.contains("$PATH"), "{error}");

    // An absent PATH is reported as such rather than silently skipped.
    let error = locate_conchd_in(Some(&missing), None, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("PATH unset"), "{error}");
}

#[test]
fn wait_for_port_sees_a_listener_and_times_out_without_one() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(wait_for_port(addr, Duration::from_secs(1)));
    drop(listener);
    // A released ephemeral port can be re-taken by anything on the machine before the
    // second probe; port 1 needs root to bind, so nothing answers there.
    let refused = "127.0.0.1:1".parse().expect("literal address");
    assert!(!wait_for_port(refused, Duration::from_millis(300)));
}

#[test]
fn pid_file_round_trips_and_detects_dead_process() {
    let dir = TempDir::new().unwrap();
    let file = PidFile {
        pid: 4_000_000_000,
        tcp: "127.0.0.1:7421".parse().unwrap(),
        http: "127.0.0.1:7420".parse().unwrap(),
    };
    fs::write(
        PidFile::path(dir.path()),
        serde_json::to_vec(&file).unwrap(),
    )
    .unwrap();
    let read = PidFile::read(dir.path()).unwrap();
    assert_eq!(read.pid, 4_000_000_000);
    assert!(!read.is_alive());
    assert!(PidFile::read(&PathBuf::from("/nonexistent")).is_none());
}

#[test]
fn the_auto_spawn_line_and_the_connect_remedy_have_one_wording() {
    assert_eq!(
        conch_launch::started_line(42, Path::new("/data")),
        "conch: started conchd (pid 42) — log: /data/conchd.log"
    );
    assert_eq!(
        conch_launch::connect_error("127.0.0.1:7421"),
        "conchd is not running on 127.0.0.1:7421. Start it with `conch up` \
         (or `brew services start conch`)."
    );
}

#[test]
fn stop_refuses_a_pid_that_is_not_a_conchd() {
    // A stale pid file whose pid has been recycled by an unrelated process: the
    // process is alive, but it is this test binary, not a conchd. `stop` must
    // never signal it.
    let file = PidFile {
        pid: std::process::id(),
        tcp: "127.0.0.1:7421".parse().unwrap(),
        http: "127.0.0.1:7420".parse().unwrap(),
    };
    assert!(file.is_alive());
    let error = file.stop(Duration::from_secs(1)).unwrap_err();
    assert!(
        matches!(error, conch_launch::LaunchError::PidMismatch { pid } if pid == file.pid),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn spawn_kills_a_daemon_that_never_listens() {
    use conch_launch::{spawn_detached, LaunchError, SpawnOptions};
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fs::create_dir(&data).unwrap();
    // A "conchd" that starts, records its pid, and never binds anything.
    let fake = dir.path().join("conchd");
    let pid_note = dir.path().join("spawned.pid");
    fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n",
            pid_note.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let refused = "127.0.0.1:1".parse().expect("literal address");
    let error = spawn_detached(&SpawnOptions {
        conchd: fake,
        data_dir: data,
        tcp: refused,
        http: refused,
    })
    .unwrap_err();
    assert!(
        matches!(error, LaunchError::NotListening { addr, .. } if addr == refused),
        "{error}"
    );
    // The half-started process must not be left behind.
    let pid: u32 = fs::read_to_string(&pid_note)
        .expect("the fake daemon ran")
        .trim()
        .parse()
        .unwrap();
    let gone = PidFile {
        pid,
        tcp: refused,
        http: refused,
    };
    assert!(!gone.is_alive(), "pid {pid} still alive after spawn failed");
}

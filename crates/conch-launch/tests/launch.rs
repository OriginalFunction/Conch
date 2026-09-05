use std::{fs, net::TcpListener, path::PathBuf, time::Duration};

use conch_launch::{locate_conchd, wait_for_port, PidFile};
use tempfile::TempDir;

struct EnvGuard {
    path: Option<std::ffi::OsString>,
    conch_conchd: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn new() -> Self {
        Self {
            path: std::env::var_os("PATH"),
            conch_conchd: std::env::var_os("CONCH_CONCHD"),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        if let Some(conch_conchd) = &self.conch_conchd {
            std::env::set_var("CONCH_CONCHD", conch_conchd);
        } else {
            std::env::remove_var("CONCH_CONCHD");
        }
    }
}

#[test]
fn locate_prefers_env_override_and_reports_search_locations() {
    let _guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let fake = dir.path().join("conchd");
    fs::write(&fake, "#!/bin/sh\n").unwrap();

    // Test 1: env override case
    std::env::set_var("CONCH_CONCHD", &fake);
    assert_eq!(locate_conchd().unwrap(), fake);

    // Test 2: missing case - temporarily clear PATH to force NotFound error
    std::env::remove_var("PATH");
    std::env::set_var("CONCH_CONCHD", "/definitely/not/here/conchd");
    let error = locate_conchd().unwrap_err().to_string();
    assert!(error.contains("/definitely/not/here/conchd"), "{error}");
}

#[test]
fn wait_for_port_sees_a_listener_and_times_out_without_one() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(wait_for_port(addr, Duration::from_secs(1)));
    drop(listener);
    assert!(!wait_for_port(addr, Duration::from_millis(300)));
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

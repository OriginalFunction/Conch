//! Locate, spawn, and stop a local `conchd`. Shared by the CLI and the MCP server.

use std::{
    env, fs,
    io::{BufRead, BufReader},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

pub const DEFAULT_TCP: &str = "127.0.0.1:7421";
pub const DEFAULT_HTTP: &str = "127.0.0.1:7420";
pub const DEFAULT_NODE: &str = "tcp://127.0.0.1:7421";
const PID_FILE: &str = "conchd.pid";
const LOG_FILE: &str = "conchd.log";

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("conchd binary not found; searched {searched}")]
    NotFound { searched: String },
    #[error("conchd did not start listening on {addr} within {secs}s; last log lines:\n{log}")]
    NotListening {
        addr: SocketAddr,
        secs: u64,
        log: String,
    },
    #[error("conchd is already running (pid {pid})")]
    AlreadyRunning { pid: u32 },
    #[error("conchd (pid {pid}) did not stop within {secs}s")]
    StopTimeout { pid: u32, secs: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn default_data_dir() -> PathBuf {
    env::var_os("CONCH_DATA_DIR").map_or_else(
        || {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".conch")
        },
        PathBuf::from,
    )
}

pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LOG_FILE)
}

/// `$CONCH_CONCHD`, then `conchd` beside the running binary, then `$PATH`.
pub fn locate_conchd() -> Result<PathBuf, LaunchError> {
    let mut searched = Vec::new();
    if let Some(explicit) = env::var_os("CONCH_CONCHD") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
        searched.push(path.display().to_string());
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("conchd");
            if sibling.is_file() {
                return Ok(sibling);
            }
            searched.push(sibling.display().to_string());
        }
    }
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join("conchd");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        searched.push("$PATH".into());
    }
    Err(LaunchError::NotFound {
        searched: searched.join(", "),
    })
}

pub fn wait_for_port(addr: SocketAddr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub struct SpawnOptions {
    pub conchd: PathBuf,
    pub data_dir: PathBuf,
    pub tcp: SocketAddr,
    pub http: SocketAddr,
}

/// Spawn `conchd --localhost` in its own session with stdout/stderr appended to the log.
/// Returns the pid once the TCP port accepts connections.
pub fn spawn_detached(options: &SpawnOptions) -> Result<u32, LaunchError> {
    if let Some(existing) = PidFile::read(&options.data_dir) {
        if existing.is_alive() {
            return Err(LaunchError::AlreadyRunning { pid: existing.pid });
        }
    }
    fs::create_dir_all(&options.data_dir)?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(&options.data_dir))?;
    let log_err = log.try_clone()?;
    let mut command = Command::new(&options.conchd);
    command
        .arg("--localhost")
        .arg("--data-dir")
        .arg(&options.data_dir)
        .arg("--tcp")
        .arg(options.tcp.to_string())
        .arg("--http")
        .arg(options.http.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    // The child outlives us; reap it in the background so it never lingers
    // as a zombie under our pid (kill -0 would otherwise still see it as
    // alive after it exits, since nothing else waits on it).
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    const SECS: u64 = 5;
    if !wait_for_port(options.tcp, Duration::from_secs(SECS)) {
        return Err(LaunchError::NotListening {
            addr: options.tcp,
            secs: SECS,
            log: tail_log(&options.data_dir, 20),
        });
    }
    Ok(pid)
}

pub fn tail_log(data_dir: &Path, lines: usize) -> String {
    let Ok(file) = fs::File::open(log_path(data_dir)) else {
        return String::new();
    };
    let all: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PidFile {
    pub pid: u32,
    pub tcp: SocketAddr,
    pub http: SocketAddr,
}

impl PidFile {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(PID_FILE)
    }

    pub fn read(data_dir: &Path) -> Option<PidFile> {
        serde_json::from_slice(&fs::read(Self::path(data_dir)).ok()?).ok()
    }

    pub fn write(&self, data_dir: &Path) -> std::io::Result<()> {
        fs::write(Self::path(data_dir), serde_json::to_vec(self)?)
    }

    pub fn remove(data_dir: &Path) {
        let _ = fs::remove_file(Self::path(data_dir));
    }

    pub fn is_alive(&self) -> bool {
        #[cfg(unix)]
        {
            // kill -0 semantics via /bin/kill keeps us libc-free.
            Command::new("kill")
                .arg("-0")
                .arg(self.pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub fn stop(&self, timeout: Duration) -> Result<(), LaunchError> {
        #[cfg(unix)]
        {
            Command::new("kill")
                .arg("-TERM")
                .arg(self.pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
        }
        let deadline = Instant::now() + timeout;
        while self.is_alive() {
            if Instant::now() >= deadline {
                return Err(LaunchError::StopTimeout {
                    pid: self.pid,
                    secs: timeout.as_secs(),
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}

# First Ten Minutes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A person installs Conch, runs `conch setup <host>` once per coding agent, and the agent can join a room on its first MCP call — with the daemon started for them, and `conch doctor` to explain the installation.

**Architecture:** A new tiny crate `conch-launch` (locate, spawn, pid file, port wait) is shared by the `conch` CLI and `conch-mcp` so both auto-spawn `conchd`. The `conch` crate gains a library target with `hosts` (data table), `edit` (byte-preserving JSON/JSONC/TOML merge), `setup`, `service`, `doctor`, and `remedy` modules; `main.rs` gains four local commands. `conchd` only writes a pid file, handles SIGTERM/Ctrl-C, and answers a `version` request.

**Tech Stack:** Rust 2021, tokio (existing), `toml_edit` 0.25 for TOML, hand-written tokenizer for JSON/JSONC, `similar` 2 for `--dry-run` diffs. Tests are cargo integration tests using `tempfile` and `HOME` overrides.

**Spec:** `docs/superpowers/specs/2026-09-05-first-ten-minutes-design.md`

## Global Constraints

- Workspace version stays `1.2.2` until release; do not bump.
- No changes to consensus, wrap, floor, ledger format, or `conch-core` scene types (spec v1.6 is law).
- macOS, LF line endings. Never run `cdk deploy`.
- Every test must pass under `cargo test --workspace --locked`; `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings` must stay clean.
- Commit messages: conventional prefix (`feat:`, `fix:`, `test:`, `docs:`, `chore:`), no AI attribution trailers.
- Default node URL is exactly `tcp://127.0.0.1:7421`; default data dir `~/.conch` (or `CONCH_DATA_DIR`).
- Host config paths, key paths, entry shapes, and skill directories are exactly the spec's host table; `--agent` defaults to `agent:<host>`.
- Config files are merged, never rewritten; unrelated bytes are preserved; a parse failure means no write.

---

## File structure

| Path | Responsibility |
|---|---|
| `crates/conch-launch/src/lib.rs` (new crate) | `locate_conchd`, `spawn_detached`, `wait_for_port`, `PidFile` read/stop, `default_data_dir`. No tokio; std only. |
| `crates/conchd/src/main.rs` | Write/remove `conchd.pid`; shutdown on SIGTERM/Ctrl-C. |
| `crates/conch-core/src/client.rs` | `ClientRequest::Version`. |
| `crates/conchd/src/tcp.rs` | Answer `Version`. |
| `crates/conch/src/lib.rs` (new) | `pub mod hosts, edit, setup, service, doctor, remedy;` |
| `crates/conch/src/hosts.rs` | `Host` enum and per-host data: config path, format, key path, entry render, skill dir, next step, fallback command. |
| `crates/conch/src/edit.rs` | `json::set_member` (JSON + JSONC, byte-preserving), `toml::set_server`, `json::strip_comments`. |
| `crates/conch/src/setup.rs` | `run(SetupOptions) -> Result<SetupReport>`: daemon check, skill write, config merge, backup, dry-run diff. |
| `crates/conch/src/service.rs` | launchd/systemd rendering, install/uninstall, Homebrew detection. |
| `crates/conch/src/doctor.rs` | Checks and report rendering. |
| `crates/conch/src/remedy.rs` | Connect-error message and per-code remedy lines. |
| `crates/conch/src/main.rs` | Parse `setup/up/down/doctor`; auto-spawn on connect; remedies on errors. |
| `crates/conch-mcp/src/lib.rs` | Auto-spawn on connect. |
| `crates/conch/tests/setup.rs`, `lifecycle.rs`, `doctor.rs` | Integration tests. |
| `crates/conchd/tests/lifecycle.rs` | Pid file, signal shutdown, version request. |
| `packaging/launchd/*.plist`, `packaging/debian/postinst` | RunAtLoad/KeepAlive true; enable+start unit. |
| `integrations/README.md`, `README.md` | Host table + Quickstart. Python integrations deleted. |

Interfaces defined once here and used by every task:

```rust
// crates/conch-launch/src/lib.rs
pub const DEFAULT_TCP: &str = "127.0.0.1:7421";
pub const DEFAULT_HTTP: &str = "127.0.0.1:7420";
pub const DEFAULT_NODE: &str = "tcp://127.0.0.1:7421";
pub fn default_data_dir() -> PathBuf;                       // CONCH_DATA_DIR or $HOME/.conch
pub fn locate_conchd() -> Result<PathBuf, LaunchError>;      // $CONCH_CONCHD, then sibling of current_exe, then PATH
pub struct SpawnOptions { pub conchd: PathBuf, pub data_dir: PathBuf, pub tcp: SocketAddr, pub http: SocketAddr }
pub fn spawn_detached(options: &SpawnOptions) -> Result<u32, LaunchError>;   // returns pid after port is listening
pub fn wait_for_port(addr: SocketAddr, timeout: Duration) -> bool;
#[derive(Serialize, Deserialize)] pub struct PidFile { pub pid: u32, pub tcp: SocketAddr, pub http: SocketAddr }
impl PidFile { pub fn path(data_dir: &Path) -> PathBuf; pub fn read(data_dir: &Path) -> Option<PidFile>; pub fn is_alive(&self) -> bool; pub fn stop(&self, timeout: Duration) -> Result<(), LaunchError>; }
pub fn tail_log(data_dir: &Path, lines: usize) -> String;
```

```rust
// crates/conch/src/hosts.rs
pub enum Host { Claude, Codex, Grok, Cursor, Gemini, Opencode }
pub enum Scope { User, Project }
pub enum Format { Json, Jsonc, Toml }
pub struct Env(pub Vec<(String, String)>);
impl Host {
    pub fn parse(name: &str) -> Option<Host>;
    pub fn name(self) -> &'static str;                                    // "claude" ...
    pub fn default_agent(self) -> String;                                 // "agent:claude"
    pub fn config_path(self, scope: Scope, home: &Path, cwd: &Path) -> PathBuf;
    pub fn format(self) -> Format;
    pub fn key_path(self) -> &'static [&'static str];                     // ["mcpServers","conch"] / ["mcp_servers","conch"] / ["mcp","conch"]
    pub fn render_json_entry(self, command: &str, args: &[String], env: &Env) -> String; // compact JSON object text
    pub fn skill_dir(self, home: &Path) -> PathBuf;                       // .../join-room
    pub fn next_step(self) -> &'static str;
    pub fn fallback_command(self, command: &str, args: &[String]) -> String;
}
pub const ALL_HOSTS: [Host; 6];
```

```rust
// crates/conch/src/edit.rs
pub mod json {
    pub fn set_member(text: &str, path: &[&str], value: &str) -> Result<String, EditError>; // JSON or JSONC
    pub fn strip_comments(text: &str) -> String;
}
pub mod toml {
    pub struct Server<'a> { pub command: &'a str, pub args: &'a [String], pub env: &'a Env, pub env_style: EnvStyle }
    pub enum EnvStyle { SubTable, Inline }
    pub fn set_server(text: &str, table: &str, name: &str, server: &Server) -> Result<String, EditError>;
}
```

```rust
// crates/conch/src/setup.rs
pub struct SetupOptions { pub host: Host, pub agent: String, pub scope: Scope, pub env: Env, pub dry_run: bool,
                          pub home: PathBuf, pub cwd: PathBuf, pub conch_binary: PathBuf, pub version: String }
pub struct SetupReport { pub config_path: PathBuf, pub config_changed: bool, pub skill_path: PathBuf, pub skill_changed: bool,
                         pub backup_path: Option<PathBuf>, pub diff: String, pub next_step: &'static str }
pub fn run(options: &SetupOptions) -> Result<SetupReport, SetupError>;   // does NOT touch the daemon; main.rs does that first
pub fn skill_text(version: &str) -> String;                              // embedded skill with version header
pub fn skill_version(text: &str) -> Option<String>;
```

---

### Task 1: `conch-launch` crate — locate, spawn, pid file

**Files:**
- Create: `crates/conch-launch/Cargo.toml`, `crates/conch-launch/src/lib.rs`, `crates/conch-launch/tests/launch.rs`
- Modify: `Cargo.toml` (workspace members + deps)

**Interfaces:**
- Produces: everything in the `conch-launch` block above.

- [ ] **Step 1: Create the crate and register it**

`Cargo.toml` (workspace): add `"crates/conch-launch"` to `members`, and under `[workspace.dependencies]` add:

```toml
toml_edit = "0.25.13"
similar = "2.7.0"
```

`crates/conch-launch/Cargo.toml`:

```toml
[package]
name = "conch-launch"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 2: Write the failing tests**

`crates/conch-launch/tests/launch.rs`:

```rust
use std::{fs, net::TcpListener, path::PathBuf, time::Duration};

use conch_launch::{locate_conchd, wait_for_port, PidFile};
use tempfile::TempDir;

#[test]
fn locate_prefers_env_override_then_sibling_then_path() {
    let dir = TempDir::new().unwrap();
    let fake = dir.path().join("conchd");
    fs::write(&fake, "#!/bin/sh\n").unwrap();
    std::env::set_var("CONCH_CONCHD", &fake);
    assert_eq!(locate_conchd().unwrap(), fake);
    std::env::remove_var("CONCH_CONCHD");
}

#[test]
fn locate_reports_both_search_locations_when_missing() {
    std::env::set_var("CONCH_CONCHD", "/definitely/not/here/conchd");
    let error = locate_conchd().unwrap_err().to_string();
    std::env::remove_var("CONCH_CONCHD");
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
    let file = PidFile { pid: 4_000_000_000, tcp: "127.0.0.1:7421".parse().unwrap(), http: "127.0.0.1:7420".parse().unwrap() };
    fs::write(PidFile::path(dir.path()), serde_json::to_vec(&file).unwrap()).unwrap();
    let read = PidFile::read(dir.path()).unwrap();
    assert_eq!(read.pid, 4_000_000_000);
    assert!(!read.is_alive());
    assert!(PidFile::read(&PathBuf::from("/nonexistent")).is_none());
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p conch-launch`
Expected: compile error — crate has no items yet.

- [ ] **Step 4: Implement the crate**

`crates/conch-launch/src/lib.rs`:

```rust
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
    NotListening { addr: SocketAddr, secs: u64, log: String },
    #[error("conchd is already running (pid {pid})")]
    AlreadyRunning { pid: u32 },
    #[error("conchd (pid {pid}) did not stop within {secs}s")]
    StopTimeout { pid: u32, secs: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn default_data_dir() -> PathBuf {
    env::var_os("CONCH_DATA_DIR").map_or_else(
        || env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".conch"),
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
    Err(LaunchError::NotFound { searched: searched.join(", ") })
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
    let log = fs::OpenOptions::new().create(true).append(true).open(log_path(&options.data_dir))?;
    let log_err = log.try_clone()?;
    let mut command = Command::new(&options.conchd);
    command
        .arg("--localhost")
        .arg("--data-dir").arg(&options.data_dir)
        .arg("--tcp").arg(options.tcp.to_string())
        .arg("--http").arg(options.http.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    let pid = child.id();
    // Do not wait on the child: it outlives us.
    std::mem::forget(child);
    const SECS: u64 = 5;
    if !wait_for_port(options.tcp, Duration::from_secs(SECS)) {
        return Err(LaunchError::NotListening { addr: options.tcp, secs: SECS, log: tail_log(&options.data_dir, 20) });
    }
    Ok(pid)
}

pub fn tail_log(data_dir: &Path, lines: usize) -> String {
    let Ok(file) = fs::File::open(log_path(data_dir)) else { return String::new() };
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
            Command::new("kill").arg("-0").arg(self.pid.to_string())
                .stdout(Stdio::null()).stderr(Stdio::null())
                .status().map(|s| s.success()).unwrap_or(false)
        }
        #[cfg(not(unix))]
        { false }
    }

    pub fn stop(&self, timeout: Duration) -> Result<(), LaunchError> {
        #[cfg(unix)]
        {
            Command::new("kill").arg("-TERM").arg(self.pid.to_string())
                .stdout(Stdio::null()).stderr(Stdio::null()).status()?;
        }
        let deadline = Instant::now() + timeout;
        while self.is_alive() {
            if Instant::now() >= deadline {
                return Err(LaunchError::StopTimeout { pid: self.pid, secs: timeout.as_secs() });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p conch-launch`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/conch-launch
git commit -m "feat: add conch-launch crate for locating and spawning conchd"
```

---

### Task 2: `conchd` pid file, signal shutdown, and `version` request

**Files:**
- Modify: `crates/conch-core/src/client.rs:84-88`
- Modify: `crates/conchd/src/tcp.rs:3071` (dispatch), `:4451-4470` (status), `:6310` (room-of)
- Modify: `crates/conchd/src/main.rs:109-131`
- Modify: `crates/conchd/Cargo.toml`
- Test: `crates/conchd/tests/lifecycle.rs`

**Interfaces:**
- Consumes: `conch_launch::PidFile`, `spawn_detached`.
- Produces: `ClientRequest::Version` → reply `{"version": "<conchd version>"}`; `<data-dir>/conchd.pid` while running.

- [ ] **Step 1: Write the failing tests**

`crates/conchd/tests/lifecycle.rs`:

```rust
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    time::Duration,
};

use conch_core::{client::{ClientReply, ClientRequest}, frame, types::AgentId};
use conch_launch::{spawn_detached, PidFile, SpawnOptions};
use conchd::tcp::Daemon;
use tempfile::TempDir;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpStream};

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

async fn request(addr: SocketAddr, request: &ClientRequest) -> ClientReply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    for message in [&ClientRequest::Attach { agent: AgentId::new("agent:test").unwrap() }, request] {
        stream.write_all(&frame::encode(message).unwrap()).await.unwrap();
        let length = stream.read_u32().await.unwrap() as usize;
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).await.unwrap();
        let reply: ClientReply = frame::decode_payload(&payload).unwrap();
        if matches!(message, ClientRequest::Attach { .. }) { assert!(reply.ok); } else { return reply; }
    }
    unreachable!()
}

#[tokio::test]
async fn version_request_reports_daemon_version() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await.unwrap();
    let reply = request(server.addr(), &ClientRequest::Version).await;
    assert!(reply.ok);
    assert_eq!(reply.data.unwrap()["version"], env!("CARGO_PKG_VERSION"));
    server.abort();
}

#[test]
fn daemon_binary_writes_pid_file_and_removes_it_on_sigterm() {
    let data = TempDir::new().unwrap();
    let options = SpawnOptions {
        conchd: PathBuf::from(env!("CARGO_BIN_EXE_conchd")),
        data_dir: data.path().to_path_buf(),
        tcp: free_port(),
        http: free_port(),
    };
    let pid = spawn_detached(&options).unwrap();
    let file = PidFile::read(data.path()).expect("pid file written at startup");
    assert_eq!(file.pid, pid);
    assert_eq!(file.tcp, options.tcp);
    assert!(file.is_alive());
    file.stop(Duration::from_secs(5)).unwrap();
    assert!(PidFile::read(data.path()).is_none(), "pid file removed on clean shutdown");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p conchd --test lifecycle`
Expected: compile error `no variant named Version` (and the crate lacks the `conch-launch` dev-dependency).

- [ ] **Step 3: Add the request variant and daemon handling**

`crates/conch-core/src/client.rs`: after the `Status` variant add:

```rust
    Version,
```

`crates/conchd/src/tcp.rs`:

- In the dispatch `match` next to line 3071 (`ClientRequest::Status { room } => self.client_status(room),`) add:

```rust
            ClientRequest::Version => Ok(json!({ "version": env!("CARGO_PKG_VERSION") })),
```

- In the room-of match near line 6310 (`ClientRequest::Status { room } => *room,`) add:

```rust
        ClientRequest::Version => None,
```

If the dispatch function returns a different wrapper type than `Result<Value, DaemonError>`, follow the exact shape used by the `Status` arm beside it.

- [ ] **Step 4: Pid file and signals in `conchd` main**

`crates/conchd/Cargo.toml`: change the tokio line to `tokio = { workspace = true, features = ["signal"] }` and add `conch-launch = { path = "../conch-launch" }` under `[dependencies]`; also add it under `[dev-dependencies]`.

`crates/conchd/src/main.rs`: replace lines 109-131 with:

```rust
    let daemon = Daemon::open(&data_dir)?;
    let client = load_client_tls(tls_ca.as_deref())?;
    let pid_file = conch_launch::PidFile { pid: std::process::id(), tcp, http };
    pid_file.write(&data_dir)?;
    let serve = async {
        if mode == TransportMode::Public {
            let cert = tls_cert.expect("validated TLS certificate");
            let key = tls_key.expect("validated TLS key");
            validate_private_key_mode(&key)?;
            let server = load_server_tls(&cert, &key)?;
            daemon.configure_transport(mode, Some(client))?;
            for endpoint in advertised {
                daemon.advertise(&endpoint)?;
            }
            tokio::try_join!(
                daemon.serve_tls(tcp, Arc::clone(&server)),
                daemon.serve_http_tls(http, server)
            )?;
        } else {
            daemon.configure_transport(mode, Some(client))?;
            for endpoint in advertised {
                daemon.advertise(&endpoint)?;
            }
            tokio::try_join!(daemon.serve(tcp), daemon.serve_http(http))?;
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    };
    let result = tokio::select! {
        result = serve => result,
        () = shutdown_signal() => Ok(()),
    };
    conch_launch::PidFile::remove(&data_dir);
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
```

`data_dir` must stay owned by `run` after `Daemon::open(&data_dir)` — `Daemon::open` takes `impl Into<PathBuf>`, so pass `data_dir.clone()` if the borrow does not compile.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p conchd --test lifecycle && cargo test -p conch-core`
Expected: both lifecycle tests pass; conch-core still green (the enum gained a unit variant; `deny_unknown_fields` tests unaffected).

- [ ] **Step 6: Commit**

```bash
git add crates/conch-core/src/client.rs crates/conchd
git commit -m "feat: conchd writes a pid file, exits on SIGTERM, and answers version"
```

---

### Task 3: `hosts` table and the `conch` library target

**Files:**
- Create: `crates/conch/src/lib.rs`, `crates/conch/src/hosts.rs`
- Modify: `crates/conch/Cargo.toml`

**Interfaces:**
- Produces: the `hosts.rs` block from File structure.

- [ ] **Step 1: Add the library target and dependencies**

`crates/conch/Cargo.toml`: add

```toml
[lib]
name = "conch"
path = "src/lib.rs"

[[bin]]
name = "conch"
path = "src/main.rs"
```

and to `[dependencies]`: `conch-launch = { path = "../conch-launch" }`, `toml_edit.workspace = true`, `similar.workspace = true`, `thiserror.workspace = true`.

`crates/conch/src/lib.rs`:

```rust
pub mod doctor;
pub mod edit;
pub mod hosts;
pub mod remedy;
pub mod service;
pub mod setup;
```

Create empty `doctor.rs`, `edit.rs`, `remedy.rs`, `service.rs`, `setup.rs` files containing only `//! filled in by later tasks` so the crate compiles; each later task replaces its file.

- [ ] **Step 2: Write the failing unit tests (inside `hosts.rs`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn config_paths_follow_the_spec_table() {
        let home = Path::new("/h");
        let cwd = Path::new("/p");
        assert_eq!(Host::Claude.config_path(Scope::User, home, cwd), Path::new("/h/.claude.json"));
        assert_eq!(Host::Codex.config_path(Scope::User, home, cwd), Path::new("/h/.codex/config.toml"));
        assert_eq!(Host::Grok.config_path(Scope::User, home, cwd), Path::new("/h/.grok/config.toml"));
        assert_eq!(Host::Cursor.config_path(Scope::User, home, cwd), Path::new("/h/.cursor/mcp.json"));
        assert_eq!(Host::Gemini.config_path(Scope::User, home, cwd), Path::new("/h/.gemini/settings.json"));
        assert_eq!(Host::Opencode.config_path(Scope::User, home, cwd), Path::new("/h/.config/opencode/opencode.json"));
        assert_eq!(Host::Claude.config_path(Scope::Project, home, cwd), Path::new("/p/.mcp.json"));
        assert_eq!(Host::Opencode.config_path(Scope::Project, home, cwd), Path::new("/p/opencode.json"));
    }

    #[test]
    fn skill_dirs_share_agents_for_four_hosts() {
        let home = Path::new("/h");
        assert_eq!(Host::Claude.skill_dir(home), Path::new("/h/.claude/skills/join-room"));
        for host in [Host::Codex, Host::Grok, Host::Cursor, Host::Gemini] {
            assert_eq!(host.skill_dir(home), Path::new("/h/.agents/skills/join-room"));
        }
        assert_eq!(Host::Opencode.skill_dir(home), Path::new("/h/.config/opencode/skills/join-room"));
    }

    #[test]
    fn json_entries_match_each_host_shape() {
        let args = vec!["--agent".to_string(), "agent:x".to_string(), "mcp".to_string()];
        let env = Env(vec![]);
        assert_eq!(Host::Claude.render_json_entry("/b/conch", &args, &env),
            r#"{"type":"stdio","command":"/b/conch","args":["--agent","agent:x","mcp"]}"#);
        assert_eq!(Host::Cursor.render_json_entry("/b/conch", &args, &env),
            r#"{"command":"/b/conch","args":["--agent","agent:x","mcp"]}"#);
        assert_eq!(Host::Opencode.render_json_entry("/b/conch", &args, &env),
            r#"{"type":"local","command":["/b/conch","--agent","agent:x","mcp"],"enabled":true}"#);
        let env = Env(vec![("CONCH_NODE".into(), "tcp://127.0.0.1:9".into())]);
        assert_eq!(Host::Gemini.render_json_entry("/b/conch", &args, &env),
            r#"{"command":"/b/conch","args":["--agent","agent:x","mcp"],"env":{"CONCH_NODE":"tcp://127.0.0.1:9"}}"#);
        assert_eq!(Host::Opencode.render_json_entry("/b/conch", &args, &env),
            r#"{"type":"local","command":["/b/conch","--agent","agent:x","mcp"],"enabled":true,"environment":{"CONCH_NODE":"tcp://127.0.0.1:9"}}"#);
    }

    #[test]
    fn default_agent_and_parse() {
        assert_eq!(Host::parse("cursor"), Some(Host::Cursor));
        assert_eq!(Host::parse("vim"), None);
        assert_eq!(Host::Gemini.default_agent(), "agent:gemini");
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p conch --lib hosts`
Expected: compile errors for missing items.

- [ ] **Step 4: Implement `hosts.rs`**

```rust
//! Per-host data for `conch setup`: where the MCP config lives, its format, and the entry shape.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host { Claude, Codex, Grok, Cursor, Gemini, Opencode }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope { User, Project }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format { Json, Jsonc, Toml }

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env(pub Vec<(String, String)>);

pub const ALL_HOSTS: [Host; 6] = [Host::Claude, Host::Codex, Host::Grok, Host::Cursor, Host::Gemini, Host::Opencode];

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialises")
}

fn json_array(items: impl Iterator<Item = String>) -> String {
    format!("[{}]", items.map(|s| json_string(&s)).collect::<Vec<_>>().join(","))
}

fn json_env(env: &Env) -> String {
    format!("{{{}}}", env.0.iter().map(|(k, v)| format!("{}:{}", json_string(k), json_string(v))).collect::<Vec<_>>().join(","))
}

impl Host {
    pub fn parse(name: &str) -> Option<Host> {
        ALL_HOSTS.into_iter().find(|host| host.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Host::Claude => "claude", Host::Codex => "codex", Host::Grok => "grok",
            Host::Cursor => "cursor", Host::Gemini => "gemini", Host::Opencode => "opencode",
        }
    }

    pub fn default_agent(self) -> String {
        format!("agent:{}", self.name())
    }

    pub fn format(self) -> Format {
        match self {
            Host::Codex | Host::Grok => Format::Toml,
            Host::Opencode => Format::Jsonc,
            Host::Claude | Host::Cursor | Host::Gemini => Format::Json,
        }
    }

    pub fn key_path(self) -> &'static [&'static str] {
        match self {
            Host::Codex | Host::Grok => &["mcp_servers", "conch"],
            Host::Opencode => &["mcp", "conch"],
            Host::Claude | Host::Cursor | Host::Gemini => &["mcpServers", "conch"],
        }
    }

    pub fn config_path(self, scope: Scope, home: &Path, cwd: &Path) -> PathBuf {
        let env_dir = |var: &str, default: PathBuf| std::env::var_os(var).map(PathBuf::from).unwrap_or(default);
        match (self, scope) {
            (Host::Claude, Scope::User) => home.join(".claude.json"),
            (Host::Claude, Scope::Project) => cwd.join(".mcp.json"),
            (Host::Codex, Scope::User) => env_dir("CODEX_HOME", home.join(".codex")).join("config.toml"),
            (Host::Codex, Scope::Project) => cwd.join(".codex/config.toml"),
            (Host::Grok, Scope::User) => env_dir("GROK_HOME", home.join(".grok")).join("config.toml"),
            (Host::Grok, Scope::Project) => cwd.join(".grok/config.toml"),
            (Host::Cursor, Scope::User) => home.join(".cursor/mcp.json"),
            (Host::Cursor, Scope::Project) => cwd.join(".cursor/mcp.json"),
            (Host::Gemini, Scope::User) => home.join(".gemini/settings.json"),
            (Host::Gemini, Scope::Project) => cwd.join(".gemini/settings.json"),
            (Host::Opencode, Scope::User) => {
                if let Some(explicit) = std::env::var_os("OPENCODE_CONFIG") {
                    return PathBuf::from(explicit);
                }
                let dir = home.join(".config/opencode");
                let jsonc = dir.join("opencode.jsonc");
                if jsonc.is_file() { jsonc } else { dir.join("opencode.json") }
            }
            (Host::Opencode, Scope::Project) => {
                let jsonc = cwd.join("opencode.jsonc");
                if jsonc.is_file() { jsonc } else { cwd.join("opencode.json") }
            }
        }
    }

    /// Compact JSON object text for the `conch` entry (JSON and JSONC hosts).
    pub fn render_json_entry(self, command: &str, args: &[String], env: &Env) -> String {
        let cmd = json_string(command);
        let argv = json_array(args.iter().cloned());
        let mut out = match self {
            Host::Claude => format!(r#"{{"type":"stdio","command":{cmd},"args":{argv}"#),
            Host::Cursor | Host::Gemini => format!(r#"{{"command":{cmd},"args":{argv}"#),
            Host::Opencode => format!(
                r#"{{"type":"local","command":{},"enabled":true"#,
                json_array(std::iter::once(command.to_string()).chain(args.iter().cloned()))
            ),
            Host::Codex | Host::Grok => unreachable!("TOML hosts use edit::toml::set_server"),
        };
        if !env.0.is_empty() {
            let key = if self == Host::Opencode { "environment" } else { "env" };
            out.push_str(&format!(r#","{key}":{}"#, json_env(env)));
        }
        out.push('}');
        out
    }

    pub fn skill_dir(self, home: &Path) -> PathBuf {
        match self {
            Host::Claude => home.join(".claude/skills/join-room"),
            Host::Opencode => home.join(".config/opencode/skills/join-room"),
            Host::Codex | Host::Grok | Host::Cursor | Host::Gemini => home.join(".agents/skills/join-room"),
        }
    }

    pub fn next_step(self) -> &'static str {
        match self {
            Host::Claude => "Restart Claude Code, then ask it to join a ticket: `join ./my-room.conch`.",
            Host::Codex => "Start a new Codex thread so it discovers the conch server and skill.",
            Host::Grok => "Start a new Grok session; `grok mcp list` shows conch.",
            Host::Cursor => "Reload the Cursor window and confirm conch appears under Settings → MCP.",
            Host::Gemini => "Restart gemini; `/mcp` lists conch.",
            Host::Opencode => "Restart opencode.",
        }
    }

    /// What to print when the config file cannot be edited safely.
    pub fn fallback_command(self, command: &str, args: &[String]) -> String {
        let argv = args.join(" ");
        match self {
            Host::Claude => format!("claude mcp add --scope user conch -- {command} {argv}"),
            Host::Codex => format!("codex mcp add conch -- {command} {argv}"),
            Host::Grok => format!("grok mcp add conch -- {command} {argv}"),
            Host::Gemini => format!("gemini mcp add -s user conch {command} {argv}"),
            Host::Cursor | Host::Opencode => format!(
                "add this to {}:\n{}",
                self.config_path(Scope::User, Path::new("~"), Path::new(".")).display(),
                self.render_json_entry(command, args, &Env::default())
            ),
        }
    }
}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p conch --lib hosts`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/conch
git commit -m "feat: host table for conch setup"
```

---

### Task 4: `edit` — byte-preserving JSON/JSONC and TOML merge

**Files:**
- Modify: `crates/conch/src/edit.rs`

**Interfaces:**
- Consumes: `hosts::Env`.
- Produces: `edit::json::set_member`, `edit::json::strip_comments`, `edit::toml::set_server`, `edit::EditError`.

- [ ] **Step 1: Write the failing unit tests (inside `edit.rs`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::Env;

    #[test]
    fn json_inserts_into_existing_object_preserving_indent_and_neighbours() {
        let input = "{\n    \"other\": 1,\n    \"mcpServers\": {\n        \"pencil\": {\"command\": \"p\"}\n    }\n}\n";
        let out = json::set_member(input, &["mcpServers", "conch"], r#"{"command":"c","args":["mcp"]}"#).unwrap();
        assert_eq!(out, "{\n    \"other\": 1,\n    \"mcpServers\": {\n        \"pencil\": {\"command\": \"p\"},\n        \"conch\": {\n            \"command\": \"c\",\n            \"args\": [\n                \"mcp\"\n            ]\n        }\n    }\n}\n");
    }

    #[test]
    fn json_replaces_existing_member_in_place() {
        let input = "{\n  \"mcpServers\": {\n    \"conch\": {\"command\": \"old\"},\n    \"z\": 1\n  }\n}\n";
        let out = json::set_member(input, &["mcpServers", "conch"], r#"{"command":"new"}"#).unwrap();
        assert_eq!(out, "{\n  \"mcpServers\": {\n    \"conch\": {\n      \"command\": \"new\"\n    },\n    \"z\": 1\n  }\n}\n");
    }

    #[test]
    fn json_creates_missing_parent_and_empty_file() {
        let out = json::set_member("{}", &["mcpServers", "conch"], r#"{"command":"c"}"#).unwrap();
        assert_eq!(out, "{\n  \"mcpServers\": {\n    \"conch\": {\n      \"command\": \"c\"\n    }\n  }\n}");
        let out = json::set_member("", &["mcp", "conch"], r#"{"type":"local"}"#).unwrap();
        assert_eq!(out, "{\n  \"mcp\": {\n    \"conch\": {\n      \"type\": \"local\"\n    }\n  }\n}\n");
    }

    #[test]
    fn jsonc_keeps_comments_and_trailing_commas_elsewhere() {
        let input = "{\n  // keep me\n  \"$schema\": \"x\",\n  \"mcp\": {\n    \"pencil\": { \"type\": \"local\" }, // trailing\n  },\n}\n";
        let out = json::set_member(input, &["mcp", "conch"], r#"{"type":"local"}"#).unwrap();
        assert!(out.contains("// keep me"));
        assert!(out.contains("// trailing"));
        assert!(out.contains("\"conch\": {\n      \"type\": \"local\"\n    }"));
        let _: serde_json::Value = serde_json::from_str(&json::strip_comments(&out).replace(",\n}", "\n}").replace(",\n  }", "\n  }")).unwrap();
    }

    #[test]
    fn json_refuses_malformed_input() {
        assert!(json::set_member("{ \"a\": ", &["a", "b"], "1").is_err());
        assert!(json::set_member("[1,2]", &["a"], "1").is_err());
    }

    #[test]
    fn toml_inserts_server_table_preserving_comments() {
        let input = "# my codex\nmodel = \"gpt\"\n\n[mcp_servers.pencil]\ncommand = \"p\"\nargs = [\"a\"]\n";
        let env = Env(vec![("K".into(), "V".into())]);
        let server = toml::Server { command: "/b/conch", args: &["--agent".into(), "agent:codex".into(), "mcp".into()], env: &env, env_style: toml::EnvStyle::SubTable };
        let out = toml::set_server(input, "mcp_servers", "conch", &server).unwrap();
        assert!(out.starts_with("# my codex\nmodel = \"gpt\"\n"));
        assert!(out.contains("[mcp_servers.pencil]\ncommand = \"p\""));
        assert!(out.contains("[mcp_servers.conch]\ncommand = \"/b/conch\"\nargs = [\"--agent\", \"agent:codex\", \"mcp\"]\n"));
        assert!(out.contains("[mcp_servers.conch.env]\nK = \"V\"\n"));
        let inline = toml::Server { env_style: toml::EnvStyle::Inline, ..server };
        let out = toml::set_server("", "mcp_servers", "conch", &inline).unwrap();
        assert!(out.contains("env = { K = \"V\" }"), "{out}");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p conch --lib edit`
Expected: compile errors.

- [ ] **Step 3: Implement `edit.rs`**

```rust
//! Byte-preserving edits of host config files. JSON/JSONC use a small scanner so unrelated
//! bytes (comments, key order, indentation) survive; TOML uses `toml_edit`.

use crate::hosts::Env;

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("config is not a JSON object: {0}")]
    Json(String),
    #[error("config is not valid TOML: {0}")]
    Toml(#[from] toml_edit::TomlError),
}

pub mod json {
    use super::EditError;

    /// Set `path` (an object member chain) to `value` (compact JSON text), creating
    /// intermediate objects. Everything outside the touched member is byte-identical.
    pub fn set_member(text: &str, path: &[&str], value: &str) -> Result<String, EditError> {
        assert!(!path.is_empty());
        let unit = detect_indent(text);
        let mut text = if text.trim().is_empty() { "{\n}\n".to_string() } else { text.to_string() };
        let mut object = root_object(&text)?;
        let mut depth_indent = String::new();
        for (index, key) in path.iter().enumerate() {
            let last = index + 1 == path.len();
            let members = members_of(&text, object)?;
            let member_indent = members.first().map(|m| line_indent(&text, m.key_start)).unwrap_or_else(|| format!("{depth_indent}{unit}"));
            match members.iter().find(|m| m.key == *key) {
                Some(m) if last => {
                    let rendered = render_value(value, &member_indent, &unit);
                    text.replace_range(m.value_start..m.value_end, &rendered);
                    return Ok(text);
                }
                Some(m) => {
                    let span = (m.value_start, m.value_end);
                    if text.as_bytes()[span.0] != b'{' {
                        return Err(EditError::Json(format!("member {key} is not an object")));
                    }
                    object = span;
                    depth_indent = member_indent;
                }
                None => {
                    let body = if last { render_value(value, &member_indent, &unit) } else { "{\n".to_string() + &member_indent + "}" };
                    let new_member = format!("\"{key}\": {body}");
                    let insert_at = insertion_point(&text, object, &members);
                    let prefix = if members.is_empty() { format!("\n{member_indent}") } else if has_trailing_comma(&text, &members, object) { format!("\n{member_indent}") } else { format!(",\n{member_indent}") };
                    let suffix = if members.is_empty() { format!("\n{depth_indent}") } else { String::new() };
                    text.insert_str(insert_at, &format!("{prefix}{new_member}{suffix}"));
                    if last {
                        return Ok(text);
                    }
                    object = root_object(&text)?; // re-scan from the root along the path so far
                    for step in &path[..=index] {
                        let ms = members_of(&text, object)?;
                        let m = ms.iter().find(|m| m.key == *step).expect("just inserted");
                        object = (m.value_start, m.value_end);
                    }
                    depth_indent = member_indent;
                }
            }
        }
        unreachable!()
    }

    /// Remove `//` and `/* */` comments (outside strings). Used for validation and doctor reads.
    pub fn strip_comments(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => { let end = string_end(bytes, i); out.push_str(&text[i..end]); i = end; }
                b'/' if bytes.get(i + 1) == Some(&b'/') => { while i < bytes.len() && bytes[i] != b'\n' { i += 1; } }
                b'/' if bytes.get(i + 1) == Some(&b'*') => { i += 2; while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') { i += 1; } i += 2; }
                b => { out.push(b as char); i += 1; }
            }
        }
        out
    }

    struct Member { key: String, key_start: usize, value_start: usize, value_end: usize }

    fn string_end(bytes: &[u8], start: usize) -> usize {
        let mut i = start + 1;
        while i < bytes.len() {
            match bytes[i] { b'\\' => i += 2, b'"' => return i + 1, _ => i += 1 }
        }
        bytes.len()
    }

    /// Skip whitespace and comments from `i`.
    fn skip_trivia(bytes: &[u8], mut i: usize) -> usize {
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
            if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'/') { while i < bytes.len() && bytes[i] != b'\n' { i += 1; } continue; }
            if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') { i += 2; while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') { i += 1; } i += 2; continue; }
            return i;
        }
    }

    /// End index (exclusive) of the value starting at `i`.
    fn value_end(bytes: &[u8], i: usize) -> Result<usize, EditError> {
        match bytes.get(i) {
            Some(b'"') => Ok(string_end(bytes, i)),
            Some(b'{') | Some(b'[') => {
                let mut depth = 0usize;
                let mut j = i;
                while j < bytes.len() {
                    match bytes[j] {
                        b'"' => { j = string_end(bytes, j); continue; }
                        b'/' => { let k = skip_trivia(bytes, j); if k != j { j = k; continue; } j += 1; continue; }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => { depth -= 1; if depth == 0 { return Ok(j + 1); } }
                        _ => {}
                    }
                    j += 1;
                }
                Err(EditError::Json("unterminated object or array".into()))
            }
            Some(_) => {
                let mut j = i;
                while j < bytes.len() && !matches!(bytes[j], b',' | b'}' | b']') && !bytes[j].is_ascii_whitespace() && bytes[j] != b'/' { j += 1; }
                Ok(j)
            }
            None => Err(EditError::Json("unexpected end of input".into())),
        }
    }

    fn root_object(text: &str) -> Result<(usize, usize), EditError> {
        let bytes = text.as_bytes();
        let start = skip_trivia(bytes, 0);
        if bytes.get(start) != Some(&b'{') { return Err(EditError::Json("top level is not an object".into())); }
        let end = value_end(bytes, start)?;
        if skip_trivia(bytes, end) != bytes.len() { return Err(EditError::Json("trailing content after object".into())); }
        Ok((start, end))
    }

    fn members_of(text: &str, object: (usize, usize)) -> Result<Vec<Member>, EditError> {
        let bytes = text.as_bytes();
        let mut members = Vec::new();
        let mut i = skip_trivia(bytes, object.0 + 1);
        while i < object.1 - 1 {
            if bytes[i] == b',' { i = skip_trivia(bytes, i + 1); continue; }
            if bytes[i] != b'"' { return Err(EditError::Json(format!("expected a key at byte {i}"))); }
            let key_start = i;
            let key_end = string_end(bytes, i);
            let key: String = serde_json::from_str(&text[key_start..key_end]).map_err(|e| EditError::Json(e.to_string()))?;
            let colon = skip_trivia(bytes, key_end);
            if bytes.get(colon) != Some(&b':') { return Err(EditError::Json(format!("expected ':' after key {key}"))); }
            let value_start = skip_trivia(bytes, colon + 1);
            let value_end = value_end(bytes, value_start)?;
            members.push(Member { key, key_start, value_start, value_end });
            i = skip_trivia(bytes, value_end);
        }
        Ok(members)
    }

    fn line_indent(text: &str, at: usize) -> String {
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        text[line_start..at].chars().take_while(|c| *c == ' ' || *c == '\t').collect()
    }

    fn detect_indent(text: &str) -> String {
        text.lines().find_map(|line| {
            let ws: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            (!ws.is_empty() && ws.len() < line.len()).then_some(ws)
        }).unwrap_or_else(|| "  ".into())
    }

    fn has_trailing_comma(text: &str, members: &[Member], object: (usize, usize)) -> bool {
        let last = members.last().expect("non-empty");
        let after = skip_trivia(text.as_bytes(), last.value_end);
        after < object.1 && text.as_bytes()[after] == b','
    }

    fn insertion_point(text: &str, object: (usize, usize), members: &[Member]) -> usize {
        match members.last() {
            None => object.0 + 1,
            Some(last) => {
                let after = skip_trivia(text.as_bytes(), last.value_end);
                if after < object.1 && text.as_bytes()[after] == b',' { after + 1 } else { last.value_end }
            }
        }
    }

    /// Pretty-print compact JSON `value` so nested lines sit at `base` + `unit` multiples.
    fn render_value(value: &str, base: &str, unit: &str) -> String {
        let parsed: serde_json::Value = serde_json::from_str(value).expect("entry text is valid JSON");
        let pretty = serde_json::to_string_pretty(&parsed).expect("serialises");
        // serde_json pretty uses two spaces; preserve member order by re-rendering from the compact text.
        let pretty = reorder_like(value, &pretty);
        pretty.lines().enumerate().map(|(n, line)| {
            let depth = line.len() - line.trim_start().len();
            let re = format!("{base}{}{}", unit.repeat(depth / 2), line.trim_start());
            if n == 0 { line.trim_start().to_string() } else { re }
        }).collect::<Vec<_>>().join("\n")
    }

    /// serde_json's Value sorts keys; the entry text from `hosts` is authoritative, so re-render
    /// objects in the order the compact text lists them.
    fn reorder_like(compact: &str, _pretty: &str) -> String {
        fn walk(bytes: &[u8], i: usize, depth: usize, out: &mut String) -> usize {
            let pad = |d: usize| "  ".repeat(d);
            match bytes[i] {
                b'{' => {
                    let mut j = skip_trivia(bytes, i + 1);
                    if bytes[j] == b'}' { out.push_str("{}"); return j + 1; }
                    out.push_str("{\n");
                    loop {
                        let key_end = string_end(bytes, j);
                        out.push_str(&pad(depth + 1)); out.push_str(&String::from_utf8_lossy(&bytes[j..key_end])); out.push_str(": ");
                        j = skip_trivia(bytes, skip_trivia(bytes, key_end) + 1);
                        j = walk(bytes, j, depth + 1, out);
                        j = skip_trivia(bytes, j);
                        if bytes[j] == b',' { out.push_str(",\n"); j = skip_trivia(bytes, j + 1); } else { out.push('\n'); break; }
                    }
                    out.push_str(&pad(depth)); out.push('}');
                    j + 1
                }
                b'[' => {
                    let mut j = skip_trivia(bytes, i + 1);
                    if bytes[j] == b']' { out.push_str("[]"); return j + 1; }
                    out.push_str("[\n");
                    loop {
                        out.push_str(&pad(depth + 1));
                        j = walk(bytes, j, depth + 1, out);
                        j = skip_trivia(bytes, j);
                        if bytes[j] == b',' { out.push_str(",\n"); j = skip_trivia(bytes, j + 1); } else { out.push('\n'); break; }
                    }
                    out.push_str(&pad(depth)); out.push(']');
                    j + 1
                }
                _ => { let end = value_end(bytes, i).expect("valid scalar"); out.push_str(&String::from_utf8_lossy(&bytes[i..end])); end }
            }
        }
        let mut out = String::new();
        walk(compact.as_bytes(), 0, 0, &mut out);
        out
    }
}

pub mod toml {
    use super::EditError;
    use crate::hosts::Env;
    use toml_edit::{value, Array, DocumentMut, InlineTable, Item, Table};

    #[derive(Clone, Copy)]
    pub enum EnvStyle { SubTable, Inline }

    pub struct Server<'a> { pub command: &'a str, pub args: &'a [String], pub env: &'a Env, pub env_style: EnvStyle }

    pub fn set_server(text: &str, table: &str, name: &str, server: &Server) -> Result<String, EditError> {
        let mut doc: DocumentMut = text.parse()?;
        let parent = doc.entry(table).or_insert(Item::Table(Table::new()));
        let parent = parent.as_table_mut().ok_or_else(|| EditError::Toml(toml_edit::TomlError::custom(format!("{table} is not a table"))))?;
        parent.set_implicit(true);
        let mut entry = Table::new();
        entry["command"] = value(server.command);
        entry["args"] = value(server.args.iter().map(|s| s.as_str()).collect::<Array>());
        if !server.env.0.is_empty() {
            match server.env_style {
                EnvStyle::SubTable => {
                    let mut env = Table::new();
                    for (k, v) in &server.env.0 { env[k.as_str()] = value(v.as_str()); }
                    entry["env"] = Item::Table(env);
                }
                EnvStyle::Inline => {
                    let mut env = InlineTable::new();
                    for (k, v) in &server.env.0 { env.insert(k, v.as_str().into()); }
                    entry["env"] = value(env);
                }
            }
        }
        parent.insert(name, Item::Table(entry));
        Ok(doc.to_string())
    }
}
```

If `toml_edit::TomlError::custom` does not exist in 0.25, replace that error construction with `EditError::Json(format!("{table} is not a table"))` (the message is what matters) — check `cargo doc -p toml_edit --open` for the constructor.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p conch --lib edit`
Expected: 6 passed. If `json_inserts_into_existing_object...` fails on whitespace only, print both strings with `{:?}` and adjust `render_value` until the expected text is produced exactly — the expected strings are the contract.

- [ ] **Step 5: Commit**

```bash
git add crates/conch/src/edit.rs
git commit -m "feat: byte-preserving JSON/JSONC/TOML config merge"
```

---

### Task 5: `setup` module and `conch setup` command

**Files:**
- Modify: `crates/conch/src/setup.rs`, `crates/conch/src/main.rs`
- Test: `crates/conch/tests/setup.rs`

**Interfaces:**
- Consumes: `hosts`, `edit`, `conch_launch::{DEFAULT_NODE, locate_conchd, spawn_detached, default_data_dir}`.
- Produces: `setup::run`, `setup::skill_text`, `setup::skill_version`, CLI `conch setup ...`.

- [ ] **Step 1: Write the failing integration tests**

`crates/conch/tests/setup.rs`:

```rust
use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

fn conch(home: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(args)
        .env("HOME", home)
        .env("CONCH_SETUP_SKIP_DAEMON", "1")
        .env_remove("CODEX_HOME").env_remove("GROK_HOME").env_remove("OPENCODE_CONFIG")
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn ok(output: &std::process::Output) -> String {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn claude_setup_in_empty_home_writes_config_skill_and_next_step() {
    let home = TempDir::new().unwrap();
    let out = ok(&conch(home.path(), home.path(), &["setup", "claude"]));
    let config = fs::read_to_string(home.path().join(".claude.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&config).unwrap();
    let entry = &parsed["mcpServers"]["conch"];
    assert_eq!(entry["type"], "stdio");
    assert_eq!(entry["command"], env!("CARGO_BIN_EXE_conch"));
    assert_eq!(entry["args"], serde_json::json!(["--agent", "agent:claude", "mcp"]));
    let skill = fs::read_to_string(home.path().join(".claude/skills/join-room/SKILL.md")).unwrap();
    assert!(skill.starts_with("---\nname: join-room\n"));
    assert!(skill.contains(&format!("<!-- conch-skill-version: {} -->", env!("CARGO_PKG_VERSION"))));
    assert!(out.contains("Restart Claude Code"), "{out}");
    assert!(!home.path().join(".claude.json.conch-bak").exists(), "no backup for a freshly created file");
}

#[test]
fn codex_setup_preserves_comments_and_other_servers_and_backs_up_once() {
    let home = TempDir::new().unwrap();
    let codex = home.path().join(".codex");
    fs::create_dir_all(&codex).unwrap();
    let original = "# mine\nmodel = \"gpt\"\n\n[mcp_servers.pencil]\ncommand = \"p\"\n";
    fs::write(codex.join("config.toml"), original).unwrap();
    ok(&conch(home.path(), home.path(), &["setup", "codex", "--agent", "agent:codex-2"]));
    let config = fs::read_to_string(codex.join("config.toml")).unwrap();
    assert!(config.starts_with("# mine\nmodel = \"gpt\"\n"));
    assert!(config.contains("[mcp_servers.pencil]\ncommand = \"p\"\n"));
    assert!(config.contains("[mcp_servers.conch]\n"));
    assert!(config.contains("args = [\"--agent\", \"agent:codex-2\", \"mcp\"]"));
    assert_eq!(fs::read_to_string(codex.join("config.toml.conch-bak")).unwrap(), original);
    assert!(home.path().join(".agents/skills/join-room/SKILL.md").is_file());

    // second run: no change, backup untouched
    let out = ok(&conch(home.path(), home.path(), &["setup", "codex", "--agent", "agent:codex-2"]));
    assert!(out.contains("already configured"), "{out}");
    assert_eq!(fs::read_to_string(codex.join("config.toml.conch-bak")).unwrap(), original);
}

#[test]
fn opencode_jsonc_keeps_comments_and_env_lands_in_environment() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join(".config/opencode");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("opencode.jsonc"), "{\n  // hi\n  \"$schema\": \"s\",\n  \"mcp\": {\n    \"pencil\": { \"type\": \"local\", \"command\": [\"p\"] }\n  }\n}\n").unwrap();
    ok(&conch(home.path(), home.path(), &["setup", "opencode", "--env", "CONCH_NODE=tcp://127.0.0.1:9"]));
    let config = fs::read_to_string(dir.join("opencode.jsonc")).unwrap();
    assert!(config.contains("// hi"));
    assert!(config.contains("\"pencil\": { \"type\": \"local\", \"command\": [\"p\"] }"));
    assert!(config.contains("\"environment\": {\n        \"CONCH_NODE\": \"tcp://127.0.0.1:9\"\n      }"), "{config}");
    assert!(!dir.join("opencode.json").exists());
}

#[test]
fn malformed_config_is_refused_and_fallback_printed() {
    let home = TempDir::new().unwrap();
    fs::write(home.path().join(".claude.json"), "{ \"mcpServers\": ").unwrap();
    let output = conch(home.path(), home.path(), &["setup", "claude"]);
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("claude mcp add --scope user conch --"), "{err}");
    assert_eq!(fs::read_to_string(home.path().join(".claude.json")).unwrap(), "{ \"mcpServers\": ");
}

#[test]
fn dry_run_writes_nothing_and_shows_a_diff() {
    let home = TempDir::new().unwrap();
    let out = ok(&conch(home.path(), home.path(), &["setup", "cursor", "--dry-run"]));
    assert!(out.contains("+  \"mcpServers\""), "{out}");
    assert!(!home.path().join(".cursor").exists());
}

#[test]
fn project_scope_targets_cwd_files() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    ok(&conch(home.path(), project.path(), &["setup", "gemini", "--scope", "project"]));
    assert!(project.path().join(".gemini/settings.json").is_file());
    assert!(!home.path().join(".gemini").exists());
}

#[test]
fn unknown_host_is_rejected() {
    let home = TempDir::new().unwrap();
    let output = conch(home.path(), home.path(), &["setup", "vim"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("claude, codex, grok, cursor, gemini, opencode"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p conch --test setup`
Expected: all fail with `unknown command: setup`.

- [ ] **Step 3: Implement `setup.rs`**

```rust
//! `conch setup <host>`: write the join-room skill and merge the MCP entry.

use std::{fs, path::{Path, PathBuf}};

use crate::{edit, hosts::{Env, Format, Host, Scope}};

const SKILL_SOURCE: &str = include_str!("../../../skills/join-room/SKILL.md");
const VERSION_MARK: &str = "<!-- conch-skill-version: ";

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("{path} could not be parsed ({source}); nothing was written.\nRegister the server yourself:\n{fallback}")]
    Unparseable { path: PathBuf, source: edit::EditError, fallback: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub struct SetupOptions {
    pub host: Host, pub agent: String, pub scope: Scope, pub env: Env, pub dry_run: bool,
    pub home: PathBuf, pub cwd: PathBuf, pub conch_binary: PathBuf, pub version: String,
}

pub struct SetupReport {
    pub config_path: PathBuf, pub config_changed: bool, pub skill_path: PathBuf, pub skill_changed: bool,
    pub backup_path: Option<PathBuf>, pub diff: String, pub next_step: &'static str,
}

/// Embedded skill with a version comment placed right after the YAML frontmatter.
pub fn skill_text(version: &str) -> String {
    let end = SKILL_SOURCE.match_indices("\n---\n").nth(0).map(|(i, _)| i + 5).unwrap_or(0);
    format!("{}{VERSION_MARK}{version} -->\n{}", &SKILL_SOURCE[..end], &SKILL_SOURCE[end..])
}

pub fn skill_version(text: &str) -> Option<String> {
    let start = text.find(VERSION_MARK)? + VERSION_MARK.len();
    Some(text[start..].split(" -->").next()?.to_string())
}

fn args_for(agent: &str) -> Vec<String> {
    vec!["--agent".into(), agent.into(), "mcp".into()]
}

pub fn run(options: &SetupOptions) -> Result<SetupReport, SetupError> {
    let host = options.host;
    let command = options.conch_binary.display().to_string();
    let args = args_for(&options.agent);
    let config_path = host.config_path(options.scope, &options.home, &options.cwd);
    let existing = match fs::read_to_string(&config_path) { Ok(text) => Some(text), Err(e) if e.kind() == std::io::ErrorKind::NotFound => None, Err(e) => return Err(e.into()) };
    let before = existing.clone().unwrap_or_default();
    let merged = match host.format() {
        Format::Json | Format::Jsonc => edit::json::set_member(&before, host.key_path(), &host.render_json_entry(&command, &args, &options.env)),
        Format::Toml => edit::toml::set_server(&before, host.key_path()[0], host.key_path()[1], &edit::toml::Server {
            command: &command, args: &args, env: &options.env,
            env_style: if host == Host::Grok { edit::toml::EnvStyle::Inline } else { edit::toml::EnvStyle::SubTable },
        }),
    }.map_err(|source| SetupError::Unparseable { path: config_path.clone(), source, fallback: host.fallback_command(&command, &args) })?;
    let config_changed = merged != before;
    let diff = similar::TextDiff::from_lines(&before, &merged)
        .unified_diff().context_radius(2)
        .header(&config_path.display().to_string(), &config_path.display().to_string()).to_string();

    let skill_path = host.skill_dir(&options.home).join("SKILL.md");
    let wanted_skill = skill_text(&options.version);
    let skill_changed = fs::read_to_string(&skill_path).ok().and_then(|t| skill_version(&t)) != Some(options.version.clone());

    let mut backup_path = None;
    if !options.dry_run {
        if config_changed {
            if let Some(original) = &existing {
                let backup = config_path.with_file_name(format!("{}.conch-bak", config_path.file_name().unwrap().to_string_lossy()));
                if !backup.exists() { fs::write(&backup, original)?; backup_path = Some(backup); }
            }
            if let Some(parent) = config_path.parent() { fs::create_dir_all(parent)?; }
            write_atomic(&config_path, &merged)?;
        }
        if skill_changed {
            fs::create_dir_all(skill_path.parent().unwrap())?;
            write_atomic(&skill_path, &wanted_skill)?;
        }
    }
    Ok(SetupReport { config_path, config_changed, skill_path, skill_changed, backup_path, diff, next_step: host.next_step() })
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("conch-tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}
```

Check that the frontmatter split is right: `SKILL_SOURCE` starts with `---\n...\n---\n`; `match_indices("\n---\n").nth(0)` finds the closing fence, and `+5` lands after its newline. Add a unit test in `setup.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_header_sits_after_frontmatter() {
        let text = skill_text("9.9.9");
        let fence_end = text.find("\n---\n").unwrap() + 5;
        assert!(text[fence_end..].starts_with("<!-- conch-skill-version: 9.9.9 -->\n# Join"), "{}", &text[fence_end..fence_end + 60]);
        assert_eq!(skill_version(&text).as_deref(), Some("9.9.9"));
    }
}
```

(If the source has a blank line after the fence, the assertion should be `starts_with("<!-- conch-skill-version: 9.9.9 -->\n\n# Join")`; match the actual file.)

- [ ] **Step 4: Wire the command in `main.rs`**

Add to `ParsedRequest`:

```rust
    Local(LocalCommand),
```

and define near it:

```rust
enum LocalCommand {
    Setup { host: conch::hosts::Host, agent: Option<String>, scope: conch::hosts::Scope, env: conch::hosts::Env, dry_run: bool },
    Up { service: bool },
    Down { service: bool },
    Doctor,
}
```

In `Arguments::parse`, before `let resolve_room`, add arms to the command match:

```rust
            "setup" => {
                let host_name = arguments.next().ok_or("setup requires a host: claude, codex, grok, cursor, gemini, opencode")?;
                let host = conch::hosts::Host::parse(&host_name)
                    .ok_or_else(|| format!("unknown host {host_name}; expected one of claude, codex, grok, cursor, gemini, opencode"))?;
                let mut agent_override = None;
                let mut scope = conch::hosts::Scope::User;
                let mut env = conch::hosts::Env::default();
                let mut dry_run = false;
                while let Some(flag) = arguments.next() {
                    match flag.as_str() {
                        "--agent" => agent_override = Some(arguments.next().ok_or("--agent requires a name")?),
                        "--scope" => scope = match arguments.next().as_deref() {
                            Some("user") => conch::hosts::Scope::User,
                            Some("project") => conch::hosts::Scope::Project,
                            _ => return Err("--scope must be user or project".into()),
                        },
                        "--env" => {
                            let pair = arguments.next().ok_or("--env requires KEY=VALUE")?;
                            let (k, v) = pair.split_once('=').ok_or("--env requires KEY=VALUE")?;
                            env.0.push((k.to_string(), v.to_string()));
                        }
                        "--dry-run" => dry_run = true,
                        _ => return Err(format!("unknown setup argument: {flag}")),
                    }
                }
                ParsedRequest::Local(LocalCommand::Setup { host, agent: agent_override, scope, env, dry_run })
            }
```

(`up`, `down`, `doctor` arms are added in Tasks 6 and 8.) Note: the global `--agent` before the command still defaults to `local`; `setup` uses its own `--agent` after the command, else `host.default_agent()`.

In `run()`, right after the `Mcp` early return, add:

```rust
    if let ParsedRequest::Local(command) = request {
        return run_local(command).await;
    }
```

and add:

```rust
async fn run_local(command: LocalCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        LocalCommand::Setup { host, agent, scope, env, dry_run } => {
            if env::var_os("CONCH_SETUP_SKIP_DAEMON").is_none() {
                ensure_daemon().await?;
            }
            let home = env::var_os("HOME").map(PathBuf::from).ok_or("HOME is not set")?;
            let report = conch::setup::run(&conch::setup::SetupOptions {
                host, agent: agent.unwrap_or_else(|| host.default_agent()), scope, env, dry_run, home,
                cwd: env::current_dir()?, conch_binary: env::current_exe()?, version: env!("CARGO_PKG_VERSION").into(),
            })?;
            if dry_run {
                print!("{}", report.diff);
                println!("(dry run) skill → {}", report.skill_path.display());
                return Ok(());
            }
            match (report.config_changed, report.skill_changed) {
                (false, false) => println!("{}: already configured ({})", host.name(), report.config_path.display()),
                _ => {
                    println!("{}: wrote {}{}", host.name(), report.config_path.display(),
                        report.backup_path.as_ref().map(|b| format!(" (backup {})", b.display())).unwrap_or_default());
                    println!("skill → {}", report.skill_path.display());
                }
            }
            println!("{}", report.next_step);
            Ok(())
        }
        _ => unreachable!("wired in later tasks"),
    }
}
```

`ensure_daemon()` is defined in Task 6; for this task add a placeholder that returns `Ok(())` and replace it in Task 6:

```rust
async fn ensure_daemon() -> Result<(), Box<dyn std::error::Error>> { Ok(()) }
```

Update `print_help` command list to include `setup` and `print_command_help` with:

```rust
        "setup" => "conch setup <claude|codex|grok|cursor|gemini|opencode> [--agent ID] [--scope user|project] [--env K=V ...] [--dry-run]\n\
             Example: conch setup claude",
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p conch --test setup && cargo test -p conch --lib`
Expected: 7 integration tests + unit tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/conch
git commit -m "feat: conch setup <host> installs the skill and MCP entry"
```

---

### Task 6: `conch up` / `conch down`, auto-spawn in CLI and MCP, remedies

**Files:**
- Modify: `crates/conch/src/remedy.rs`, `crates/conch/src/main.rs`, `crates/conch-mcp/src/lib.rs`, `crates/conch-mcp/Cargo.toml`
- Test: `crates/conch/tests/lifecycle.rs`; modify `crates/conch/tests/cli_floor.rs`

**Interfaces:**
- Consumes: `conch_launch::*`.
- Produces: `remedy::connect_error(node: &str) -> String`, `remedy::for_code(code: &str, command: &str) -> Option<&'static str>`, `conch_mcp::run(node, agent, room, tls_ca, auto_spawn: bool)`.

- [ ] **Step 1: Write the failing tests**

`crates/conch/tests/lifecycle.rs`:

```rust
use std::{fs, net::TcpListener, path::PathBuf, process::Command, time::Duration};

use conch_launch::PidFile;
use tempfile::TempDir;

/// conchd built by the workspace; fall back to building it so `cargo test -p conch` works alone.
fn conchd_binary() -> PathBuf {
    let sibling = PathBuf::from(env!("CARGO_BIN_EXE_conch")).with_file_name("conchd");
    if !sibling.is_file() {
        let status = Command::new(env!("CARGO")).args(["build", "-p", "conchd", "--bin", "conchd"]).status().unwrap();
        assert!(status.success());
    }
    sibling
}

fn free_port() -> u16 { TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() }

fn conch(data: &TempDir, tcp: u16, http: u16, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(args)
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", format!("127.0.0.1:{tcp}"))
        .env("CONCH_DEFAULT_HTTP", format!("127.0.0.1:{http}"))
        .env_remove("CONCH_NODE")
        .output().unwrap()
}

#[test]
fn up_spawns_and_down_stops() {
    let data = TempDir::new().unwrap();
    let (tcp, http) = (free_port(), free_port());
    let up = conch(&data, tcp, http, &["up"]);
    assert!(up.status.success(), "{}", String::from_utf8_lossy(&up.stderr));
    let out = String::from_utf8_lossy(&up.stdout);
    assert!(out.contains(&format!("http://127.0.0.1:{http}/")), "{out}");
    let pid = PidFile::read(data.path()).unwrap();
    assert!(pid.is_alive());
    let again = conch(&data, tcp, http, &["up"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already running"));
    let down = conch(&data, tcp, http, &["down"]);
    assert!(down.status.success(), "{}", String::from_utf8_lossy(&down.stderr));
    assert!(PidFile::read(data.path()).is_none());
}

#[test]
fn status_auto_spawns_on_default_node_and_says_so() {
    let data = TempDir::new().unwrap();
    let (tcp, http) = (free_port(), free_port());
    let status = conch(&data, tcp, http, &["status"]);
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    assert!(String::from_utf8_lossy(&status.stderr).contains("conch: started conchd (pid "));
    assert!(String::from_utf8_lossy(&status.stdout).contains("\"rooms\""));
    PidFile::read(data.path()).unwrap().stop(Duration::from_secs(5)).unwrap();
}

#[test]
fn explicit_node_never_spawns_and_prints_remedy() {
    let data = TempDir::new().unwrap();
    let dead = free_port();
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["--node", &format!("tcp://127.0.0.1:{dead}"), "status"])
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .output().unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains(&format!("conchd is not running on 127.0.0.1:{dead}")), "{err}");
    assert!(err.contains("`conch up`"));
    assert!(PidFile::read(data.path()).is_none());
    assert!(!fs::exists(data.path().join("conchd.log")).unwrap_or(false));
}
```

Add to `crates/conch/tests/cli_floor.rs`, at the assertion near line 147 that checks `no_grant`, an extra assertion on the same stderr:

```rust
    assert!(stderr.contains("raise your hand and wait for the floor"), "{stderr}");
```

(Locate the existing `no_grant` assertion and reuse its stderr variable name.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p conch --test lifecycle`
Expected: `unknown command: up`.

- [ ] **Step 3: Implement `remedy.rs`**

```rust
//! Human remedies for the errors a new user hits first.

pub fn connect_error(node_addr: &str) -> String {
    format!("conchd is not running on {node_addr}. Start it with `conch up` (or `brew services start conch`).")
}

/// A second line for a wire error, keyed by error code and the CLI command that produced it.
pub fn for_code(code: &str, command: &str) -> Option<&'static str> {
    Some(match (code, command) {
        ("no_grant", _) => "raise your hand and wait for the floor: `conch raise-hand && conch wait-for-floor`",
        ("unknown_room", _) => "join it first: `conch join <ticket>`",
        ("not_moderator", _) => "this room is in stick mode; `grant`/`yank` need `conch config --mode moderator`",
        ("timeout", "wait-for-floor") => "your hand stays raised for 24 h; run `conch wait-for-floor` again",
        ("unavailable", "join") => "no peer could provide the room; check the ticket still carries its token",
        _ => return None,
    })
}
```

- [ ] **Step 4: Auto-spawn and `up`/`down` in `main.rs`**

Add a field `node_is_default: bool` to `Arguments`, set in `parse` as `env::var_os("CONCH_NODE").is_none() && <no --node seen>` (track with a local `let mut node_explicit = false;` set true in the `--node` arm). Also store the command name: add `command: String` to `Arguments`.

Test hooks (only read from env, documented as test-only): the default TCP/HTTP addresses used for spawning come from `CONCH_DEFAULT_TCP` / `CONCH_DEFAULT_HTTP` when set, else `conch_launch::DEFAULT_TCP` / `DEFAULT_HTTP`; and when `CONCH_DEFAULT_TCP` is set, the default node URL becomes `tcp://<that>`. Implement:

```rust
fn default_tcp() -> String { env::var("CONCH_DEFAULT_TCP").unwrap_or_else(|_| conch_launch::DEFAULT_TCP.into()) }
fn default_http() -> String { env::var("CONCH_DEFAULT_HTTP").unwrap_or_else(|_| conch_launch::DEFAULT_HTTP.into()) }
```

and in `parse` change the node default to `env::var("CONCH_NODE").unwrap_or_else(|_| format!("tcp://{}", default_tcp()))`.

Replace line 53 (`let mut stream = TcpStream::connect(...)`) with:

```rust
    let mut stream = connect_with_spawn(&node, node_is_default).await?;
```

and add:

```rust
async fn connect_with_spawn(node: &str, node_is_default: bool) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let addr = parse_node_addr(node)?;
    match TcpStream::connect(addr).await {
        Ok(stream) => Ok(stream),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused && node_is_default => {
            ensure_daemon().await?;
            Ok(TcpStream::connect(addr).await?)
        }
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => Err(conch::remedy::connect_error(&addr.to_string()).into()),
        Err(error) => Err(error.into()),
    }
}

fn spawn_options() -> Result<conch_launch::SpawnOptions, Box<dyn std::error::Error>> {
    Ok(conch_launch::SpawnOptions {
        conchd: conch_launch::locate_conchd()?,
        data_dir: conch_launch::default_data_dir(),
        tcp: default_tcp().parse()?,
        http: default_http().parse()?,
    })
}

/// Start conchd if nothing is listening on the default node. Prints one stderr line when it does.
async fn ensure_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let options = spawn_options()?;
    if conch_launch::wait_for_port(options.tcp, std::time::Duration::from_millis(200)) {
        return Ok(());
    }
    let pid = tokio::task::spawn_blocking(move || conch_launch::spawn_detached(&options)).await??;
    eprintln!("conch: started conchd (pid {pid}) — log: {}", conch_launch::log_path(&conch_launch::default_data_dir()).display());
    Ok(())
}
```

Replace the placeholder `ensure_daemon` from Task 5 with this one.

Add the `up`/`down` parse arms:

```rust
            "up" | "down" => {
                let mut service = false;
                while let Some(flag) = arguments.next() {
                    match flag.as_str() {
                        "--service" => service = true,
                        _ => return Err(format!("unknown {command} argument: {flag}")),
                    }
                }
                ParsedRequest::Local(if command == "up" { LocalCommand::Up { service } } else { LocalCommand::Down { service } })
            }
```

and in `run_local`:

```rust
        LocalCommand::Up { service } => {
            let options = spawn_options()?;
            let data_dir = options.data_dir.clone();
            let http = options.http;
            let pid = tokio::task::spawn_blocking(move || conch_launch::spawn_detached(&options)).await??;
            println!("conchd running (pid {pid})\nlog: {}\nui:  http://{http}/", conch_launch::log_path(&data_dir).display());
            if service { conch::service::install(&conch_launch::locate_conchd()?, &data_dir)?; }
            Ok(())
        }
        LocalCommand::Down { service } => {
            let data_dir = conch_launch::default_data_dir();
            match conch_launch::PidFile::read(&data_dir) {
                Some(pid) if pid.is_alive() => { pid.stop(std::time::Duration::from_secs(5))?; println!("conchd stopped (pid {})", pid.pid); }
                _ => println!("conchd is not running"),
            }
            if service { conch::service::uninstall(&data_dir)?; }
            Ok(())
        }
```

(`conch::service::install/uninstall` come in Task 7; until then leave the `if service` lines out and add them in Task 7.)

Remedy lines: change `format_reply_error` to take the command name:

```rust
fn format_reply_error(reply: &ClientReply, command: &str) -> String {
    reply.error.as_ref().map_or_else(
        || "daemon returned an unspecified error".into(),
        |error| match conch::remedy::for_code(&error.code, command) {
            Some(remedy) => format!("{}: {}\n{remedy}", error.code, error.message),
            None => format!("{}: {}", error.code, error.message),
        },
    )
}
```

and pass `&command` at its three call sites in `run` (the `command` string now lives in `Arguments`).

Update `print_help` and `print_command_help` (`"up" => "conch up [--service]\nExample: conch up --service   # start now and on login"`, `"down" => "conch down [--service]"`).

- [ ] **Step 5: Auto-spawn in `conch-mcp`**

`crates/conch-mcp/Cargo.toml`: add `conch-launch = { path = "../conch-launch" }`.

`crates/conch-mcp/src/lib.rs`: change `pub async fn run(node, agent, room, tls_ca)` to add a final parameter `auto_spawn: bool`, store it on the server struct, and at the `TcpStream::connect(parse_node_addr(&self.node)?)` site (line 369) replace with:

```rust
        let addr = parse_node_addr(&self.node)?;
        let mut stream = match TcpStream::connect(addr).await {
            Ok(stream) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused && self.auto_spawn && !self.spawned.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                let data_dir = conch_launch::default_data_dir();
                let options = conch_launch::SpawnOptions { conchd: conch_launch::locate_conchd().map_err(|e| e.to_string())?, data_dir: data_dir.clone(), tcp: addr, http: conch_launch::DEFAULT_HTTP.parse().expect("literal") };
                let pid = tokio::task::spawn_blocking(move || conch_launch::spawn_detached(&options)).await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
                eprintln!("conch: started conchd (pid {pid}) — log: {}", conch_launch::log_path(&data_dir).display());
                TcpStream::connect(addr).await.map_err(|e| e.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
```

with `spawned: std::sync::atomic::AtomicBool` added to the struct (initialised `false`). In `conch/src/main.rs` pass `node_is_default` as the new argument. Fix every other caller of `conch_mcp::run` (grep `conch_mcp::run` in `crates/*/tests`); pass `false` in tests.

- [ ] **Step 6: Run to verify pass**

Run: `cargo test -p conch --test lifecycle --test cli_floor && cargo test -p conch-mcp`
Expected: all pass. The `up_spawns_and_down_stops` test proves the pid file and stop path; `status_auto_spawns...` proves the stderr line.

- [ ] **Step 7: Commit**

```bash
git add crates/conch crates/conch-mcp
git commit -m "feat: conch up/down, auto-spawn conchd on the default node, error remedies"
```

---

### Task 7: `service` — launchd/systemd units and Homebrew detection

**Files:**
- Modify: `crates/conch/src/service.rs`, `crates/conch/src/main.rs` (the two `if service` lines from Task 6)
- Modify: `packaging/launchd/com.conch.conchd.plist`, `packaging/debian/postinst`
- Test: unit tests inside `service.rs`

**Interfaces:**
- Produces: `service::{render_launchd, render_systemd, is_homebrew, unit_path, install, uninstall}`.

- [ ] **Step 1: Write the failing unit tests (inside `service.rs`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn launchd_plist_runs_at_load_and_keeps_alive() {
        let plist = render_launchd(Path::new("/opt/bin/conchd"), Path::new("/home/u/.conch"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(plist.contains("<string>/opt/bin/conchd</string>\n    <string>--localhost</string>\n    <string>--data-dir</string>\n    <string>/home/u/.conch</string>"));
        assert!(plist.contains("<string>/home/u/.conch/conchd.log</string>"));
    }

    #[test]
    fn systemd_unit_restarts_and_points_at_binary() {
        let unit = render_systemd(Path::new("/opt/bin/conchd"), Path::new("/home/u/.conch"));
        assert!(unit.contains("ExecStart=/opt/bin/conchd --localhost --data-dir /home/u/.conch"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn homebrew_prefixes_are_detected() {
        assert!(is_homebrew(Path::new("/opt/homebrew/bin/conchd")));
        assert!(is_homebrew(Path::new("/usr/local/Cellar/conch/1.2.2/bin/conchd")));
        assert!(!is_homebrew(Path::new("/Users/me/.local/bin/conchd")));
    }

    #[test]
    fn unit_paths_are_user_level() {
        let home = Path::new("/h");
        #[cfg(target_os = "macos")]
        assert_eq!(unit_path(home), Path::new("/h/Library/LaunchAgents/com.conch.conchd.plist"));
        #[cfg(target_os = "linux")]
        assert_eq!(unit_path(home), Path::new("/h/.config/systemd/user/conchd.service"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p conch --lib service`
Expected: compile errors.

- [ ] **Step 3: Implement `service.rs`**

```rust
//! User-level service units so conchd survives logout/reboot.

use std::{fs, path::{Path, PathBuf}, process::Command};

pub fn render_launchd(conchd: &Path, data_dir: &Path) -> String {
    format!(
"<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
  <key>Label</key>
  <string>com.conch.conchd</string>
  <key>ProgramArguments</key>
  <array>
    <string>{conchd}</string>
    <string>--localhost</string>
    <string>--data-dir</string>
    <string>{data}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
", conchd = conchd.display(), data = data_dir.display(), log = data_dir.join("conchd.log").display())
}

pub fn render_systemd(conchd: &Path, data_dir: &Path) -> String {
    format!(
"[Unit]
Description=Conch room daemon
After=network.target

[Service]
ExecStart={conchd} --localhost --data-dir {data}
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
", conchd = conchd.display(), data = data_dir.display())
}

pub fn is_homebrew(conchd: &Path) -> bool {
    let text = conchd.to_string_lossy();
    text.starts_with("/opt/homebrew/") || text.contains("/Cellar/") || text.starts_with("/home/linuxbrew/")
}

pub fn unit_path(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents/com.conch.conchd.plist")
    } else {
        home.join(".config/systemd/user/conchd.service")
    }
}

fn home() -> std::io::Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| std::io::Error::other("HOME is not set"))
}

fn run(program: &str, args: &[String]) -> std::io::Result<()> {
    let status = Command::new(program).args(args).status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("{program} {} failed with {status}", args.join(" ")))) }
}

/// Install and start the user-level unit, or explain the Homebrew path.
pub fn install(conchd: &Path, data_dir: &Path) -> std::io::Result<()> {
    if is_homebrew(conchd) {
        println!("conchd comes from Homebrew; run: brew services start conch");
        return Ok(());
    }
    let unit = unit_path(&home()?);
    fs::create_dir_all(unit.parent().unwrap())?;
    if cfg!(target_os = "macos") {
        fs::write(&unit, render_launchd(conchd, data_dir))?;
        let uid = String::from_utf8_lossy(&Command::new("id").arg("-u").output()?.stdout).trim().to_string();
        let _ = run("launchctl", &["bootout".into(), format!("gui/{uid}/com.conch.conchd")]);
        run("launchctl", &["bootstrap".into(), format!("gui/{uid}"), unit.display().to_string()])?;
    } else {
        fs::write(&unit, render_systemd(conchd, data_dir))?;
        run("systemctl", &["--user".into(), "daemon-reload".into()])?;
        run("systemctl", &["--user".into(), "enable".into(), "--now".into(), "conchd".into()])?;
    }
    println!("service installed: {}", unit.display());
    Ok(())
}

pub fn uninstall(_data_dir: &Path) -> std::io::Result<()> {
    let unit = unit_path(&home()?);
    if !unit.exists() {
        println!("no conch service unit at {}", unit.display());
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let uid = String::from_utf8_lossy(&Command::new("id").arg("-u").output()?.stdout).trim().to_string();
        let _ = run("launchctl", &["bootout".into(), format!("gui/{uid}/com.conch.conchd")]);
    } else {
        let _ = run("systemctl", &["--user".into(), "disable".into(), "--now".into(), "conchd".into()]);
    }
    fs::remove_file(&unit)?;
    println!("service removed: {}", unit.display());
    Ok(())
}

/// For `doctor`: is a unit file present for this user?
pub fn unit_installed() -> bool {
    home().map(|h| unit_path(&h).exists()).unwrap_or(false)
}
```

Add the two `if service { ... }` lines back into `run_local` in `main.rs` (Task 6 left them out). When `spawn_detached` fails with `AlreadyRunning` and `--service` was requested, still call `install` — handle it as `Err(conch_launch::LaunchError::AlreadyRunning { .. }) if service => {}` before the `?`.

- [ ] **Step 4: Fix the packaged units**

`packaging/launchd/com.conch.conchd.plist`: change both `<false/>` to `<true/>`.

`packaging/debian/postinst`: after the existing `daemon-reload`, add:

```sh
if command -v systemctl >/dev/null 2>&1; then
  systemctl enable --now conchd.service >/dev/null 2>&1 || true
fi
```

Run `bash scripts/check-packaging.sh` and fix whatever it flags about the changed files.

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p conch --lib service && bash scripts/check-packaging.sh`
Expected: 4 unit tests pass; packaging check passes.

- [ ] **Step 6: Commit**

```bash
git add crates/conch packaging
git commit -m "feat: conch up --service installs a user-level launchd/systemd unit"
```

---

### Task 8: `conch doctor`

**Files:**
- Modify: `crates/conch/src/doctor.rs`, `crates/conch/src/main.rs`
- Test: `crates/conch/tests/doctor.rs`

**Interfaces:**
- Consumes: `conch_launch`, `hosts`, `edit::json::strip_comments`, `setup::skill_version`, `service::{unit_installed, is_homebrew}`, `ClientRequest::Version`.
- Produces: `doctor::{Check, Level, run_checks(DoctorInput) -> Vec<Check>, render(&[Check]) -> String}`; CLI `conch doctor` exits 1 on any `fail`.

- [ ] **Step 1: Write the failing integration tests**

`crates/conch/tests/doctor.rs`:

```rust
use std::{fs, net::TcpListener, path::PathBuf, process::Command, time::Duration};

use conch_launch::PidFile;
use tempfile::TempDir;

fn conchd_binary() -> PathBuf {
    let sibling = PathBuf::from(env!("CARGO_BIN_EXE_conch")).with_file_name("conchd");
    if !sibling.is_file() {
        assert!(Command::new(env!("CARGO")).args(["build", "-p", "conchd", "--bin", "conchd"]).status().unwrap().success());
    }
    sibling
}

fn free_port() -> u16 { TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() }

fn doctor(home: &TempDir, data: &TempDir, tcp: u16) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_conch"))
        .arg("doctor")
        .env("HOME", home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", format!("127.0.0.1:{tcp}"))
        .env("CONCH_DEFAULT_HTTP", format!("127.0.0.1:{}", free_port()))
        .env("CONCH_DOCTOR_NO_SPAWN", "1")
        .env_remove("CONCH_NODE")
        .output().unwrap()
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
    assert!(Command::new(env!("CARGO_BIN_EXE_conch")).args(["setup", "cursor"]).env("HOME", home.path()).env("CONCH_SETUP_SKIP_DAEMON", "1").status().unwrap().success());
    assert!(Command::new(env!("CARGO_BIN_EXE_conch")).arg("up").env("CONCH_DATA_DIR", data.path()).env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", format!("127.0.0.1:{tcp}")).env("CONCH_DEFAULT_HTTP", format!("127.0.0.1:{}", free_port())).env_remove("CONCH_NODE").status().unwrap().success());
    let output = doctor(&home, &data, tcp);
    let out = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(out.contains(&format!("ok    daemon        reachable on 127.0.0.1:{tcp}, version {}", env!("CARGO_PKG_VERSION"))), "{out}");
    assert!(out.contains("warn  daemon        started by hand"), "{out}");
    assert!(out.contains("ok    cursor        agent:cursor"), "{out}");
    assert!(out.contains("--    claude        not configured"), "{out}");
    PidFile::read(data.path()).unwrap().stop(Duration::from_secs(5)).unwrap();
}

#[test]
fn doctor_flags_a_stale_skill() {
    let (home, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    assert!(Command::new(env!("CARGO_BIN_EXE_conch")).args(["setup", "claude"]).env("HOME", home.path()).env("CONCH_SETUP_SKIP_DAEMON", "1").status().unwrap().success());
    let skill = home.path().join(".claude/skills/join-room/SKILL.md");
    fs::write(&skill, fs::read_to_string(&skill).unwrap().replace(env!("CARGO_PKG_VERSION"), "0.0.1")).unwrap();
    let out = String::from_utf8_lossy(&doctor(&home, &data, free_port()).stdout).into_owned();
    assert!(out.contains("warn  claude        agent:claude, skill 0.0.1 is stale"), "{out}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p conch --test doctor`
Expected: `unknown command: doctor`.

- [ ] **Step 3: Implement `doctor.rs`**

```rust
//! `conch doctor`: one line per check, remedies inline, exit 1 on any failure.

use std::{env, fs, net::SocketAddr, path::{Path, PathBuf}};

use crate::{edit, hosts::{Format, Host, Scope, ALL_HOSTS}, service, setup};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level { Ok, Warn, Fail, Skip }

pub struct Check { pub level: Level, pub name: &'static str, pub detail: String, pub remedy: Option<String> }

pub struct DoctorInput {
    pub cli_version: String,
    pub conch_binary: PathBuf,
    pub daemon: DaemonProbe,
    pub data_dir: PathBuf,
    pub home: PathBuf,
    pub current_room: Option<String>,
}

pub enum DaemonProbe { Unreachable { addr: SocketAddr }, Reachable { addr: SocketAddr, version: Option<String> } }

fn check(level: Level, name: &'static str, detail: impl Into<String>, remedy: Option<String>) -> Check {
    Check { level, name, detail: detail.into(), remedy }
}

pub fn run_checks(input: &DoctorInput) -> Vec<Check> {
    let mut out = Vec::new();
    out.push(check(Level::Ok, "conch", format!("{} version {}", input.conch_binary.display(), input.cli_version), None));

    let duplicates = duplicates_on_path(&input.conch_binary);
    if !duplicates.is_empty() {
        out.push(check(Level::Warn, "installs", format!("other conch binaries on PATH: {}", duplicates.join(", ")), Some("remove the one you do not use so `conch` and `conchd` versions match".into())));
    }

    match &input.daemon {
        DaemonProbe::Unreachable { addr } => out.push(check(Level::Fail, "daemon", format!("not running on {addr}"), Some("run `conch up` (or `brew services start conch`)".into()))),
        DaemonProbe::Reachable { addr, version } => {
            match version {
                Some(v) if *v == input.cli_version => out.push(check(Level::Ok, "daemon", format!("reachable on {addr}, version {v}"), None)),
                Some(v) => out.push(check(Level::Warn, "daemon", format!("reachable on {addr}, version {v} (conch is {})", input.cli_version), Some("restart it: `conch down && conch up`".into()))),
                None => out.push(check(Level::Warn, "daemon", format!("reachable on {addr}, version unknown (older than 1.3)"), Some("restart it after upgrading: `conch down && conch up`".into()))),
            }
            let conchd = conch_launch::PidFile::read(&input.data_dir);
            let how = if service::unit_installed() { "service unit installed" }
                else if conchd.is_some() && service::is_homebrew(&conch_launch::locate_conchd().unwrap_or_default()) { "Homebrew service" }
                else if conchd.is_some() { "started by hand (pid file only)" }
                else { "unknown (no pid file)" };
            let level = if how.starts_with("started by hand") || how.starts_with("unknown") { Level::Warn } else { Level::Ok };
            out.push(check(level, "daemon", how, (level == Level::Warn).then(|| "it will not survive a reboot; run `conch up --service`".into())));
        }
    }

    match fs::metadata(&input.data_dir) {
        Ok(meta) => {
            #[cfg(unix)]
            let mode = { use std::os::unix::fs::PermissionsExt; meta.permissions().mode() & 0o777 };
            #[cfg(not(unix))]
            let mode = 0o700;
            if mode & 0o077 != 0 {
                out.push(check(Level::Warn, "data dir", format!("{} mode {mode:o}", input.data_dir.display()), Some(format!("chmod 700 {}", input.data_dir.display()))));
            } else {
                out.push(check(Level::Ok, "data dir", input.data_dir.display().to_string(), None));
            }
        }
        Err(_) => out.push(check(Level::Warn, "data dir", format!("{} does not exist yet", input.data_dir.display()), Some("it is created on first `conch up`".into()))),
    }

    match &input.current_room {
        Some(room) => out.push(check(Level::Ok, "current room", room.clone(), None)),
        None => out.push(check(Level::Skip, "current room", "none (create or join one)", None)),
    }

    for host in ALL_HOSTS {
        out.push(host_check(host, &input.home, &input.cli_version));
    }
    out
}

fn host_check(host: Host, home: &Path, cli_version: &str) -> Check {
    let path = host.config_path(Scope::User, home, Path::new("."));
    let Ok(text) = fs::read_to_string(&path) else { return check(Level::Skip, host.name(), "not configured", None) };
    let agent = match host.format() {
        Format::Toml => text.parse::<toml_edit::DocumentMut>().ok().and_then(|doc| {
            let args = doc.get("mcp_servers")?.get("conch")?.get("args")?.as_array()?.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
            agent_from_args(&args)
        }),
        Format::Json | Format::Jsonc => serde_json::from_str::<serde_json::Value>(&edit::json::strip_comments(&text)).ok().and_then(|v| {
            let entry = v.get(host.key_path()[0])?.get("conch")?;
            let args: Vec<String> = entry.get("args").or_else(|| entry.get("command")).and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            agent_from_args(&args)
        }),
    };
    let Some(agent) = agent else {
        let legacy = host == Host::Codex && text.contains("plugins.\"conch@");
        return if legacy { check(Level::Warn, host.name(), "legacy Codex plugin entry only", Some("run `conch setup codex` to migrate".into())) } else { check(Level::Skip, host.name(), "not configured", None) };
    };
    let skill = fs::read_to_string(host.skill_dir(home).join("SKILL.md")).ok();
    match skill.as_deref().and_then(setup::skill_version) {
        Some(v) if v == cli_version => check(Level::Ok, host.name(), agent, None),
        Some(v) => check(Level::Warn, host.name(), format!("{agent}, skill {v} is stale"), Some(format!("run `conch setup {}`", host.name()))),
        None => check(Level::Warn, host.name(), format!("{agent}, skill missing"), Some(format!("run `conch setup {}`", host.name()))),
    }
}

fn agent_from_args(args: &[String]) -> Option<String> {
    let index = args.iter().position(|a| a == "--agent")?;
    args.get(index + 1).cloned()
}

fn duplicates_on_path(current: &Path) -> Vec<String> {
    let canonical = fs::canonicalize(current).ok();
    env::var_os("PATH").map(|path| env::split_paths(&path)
        .map(|dir| dir.join("conch"))
        .filter(|candidate| candidate.is_file() && fs::canonicalize(candidate).ok() != canonical)
        .map(|p| p.display().to_string()).collect()).unwrap_or_default()
}

pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        let tag = match c.level { Level::Ok => "ok  ", Level::Warn => "warn", Level::Fail => "fail", Level::Skip => "--  " };
        out.push_str(&format!("{tag}  {:<13} {}\n", c.name, c.detail));
        if let Some(remedy) = &c.remedy { out.push_str(&format!("      {:<13} → {remedy}\n", "")); }
    }
    out
}

pub fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.level == Level::Fail)
}
```

`conch_launch::locate_conchd().unwrap_or_default()` needs `PathBuf: Default` — it is; an empty path is simply "not Homebrew".

- [ ] **Step 4: Wire `doctor` in `main.rs`**

Parse arm: `"doctor" => ParsedRequest::Local(LocalCommand::Doctor),` (reject trailing flags with `unknown doctor argument`).

In `run_local`:

```rust
        LocalCommand::Doctor => {
            let addr: SocketAddr = default_tcp().parse()?;
            let daemon = if conch_launch::wait_for_port(addr, std::time::Duration::from_millis(300)) {
                let version = probe_version(addr).await;
                conch::doctor::DaemonProbe::Reachable { addr, version }
            } else {
                conch::doctor::DaemonProbe::Unreachable { addr }
            };
            let checks = conch::doctor::run_checks(&conch::doctor::DoctorInput {
                cli_version: env!("CARGO_PKG_VERSION").into(),
                conch_binary: env::current_exe()?,
                daemon,
                data_dir: conch_launch::default_data_dir(),
                home: env::var_os("HOME").map(PathBuf::from).ok_or("HOME is not set")?,
                current_room: read_current_room(),
            });
            print!("{}", conch::doctor::render(&checks));
            if conch::doctor::failed(&checks) { std::process::exit(1); }
            Ok(())
        }
```

and:

```rust
/// Ask a reachable daemon for its version; `None` when it predates the request.
async fn probe_version(addr: SocketAddr) -> Option<String> {
    let mut stream = TcpStream::connect(addr).await.ok()?;
    write_frame(&mut stream, &ClientRequest::Attach { agent: AgentId::new("agent:doctor").ok()? }).await.ok()?;
    let attached: ClientReply = read_frame(&mut stream).await.ok()?;
    if !attached.ok { return None; }
    write_frame(&mut stream, &ClientRequest::Version).await.ok()?;
    let reply: ClientReply = read_frame(&mut stream).await.ok()?;
    reply.data?.get("version")?.as_str().map(String::from)
}
```

`doctor` never auto-spawns (it reports instead). `CONCH_DOCTOR_NO_SPAWN` in the tests is therefore unused by the implementation; delete it from the tests once you confirm `doctor` does not call `ensure_daemon`.

Help: `"doctor" => "conch doctor\nExample: conch doctor   # exit 1 if anything is red"`, and add `setup, up, down, doctor` to the root help command list.

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p conch --test doctor`
Expected: 3 passed. If column alignment differs from the expected strings, adjust `render`'s widths so `ok    daemon        reachable` (4-char tag, two spaces, name padded to 13, space) matches exactly.

- [ ] **Step 6: Commit**

```bash
git add crates/conch
git commit -m "feat: conch doctor reports binary, daemon, data dir, and host setup state"
```

---

### Task 9: Remove the Python integrations and update docs/site

**Files:**
- Delete: `integrations/install.py`, `integrations/validate.py`, `integrations/tests/`, `integrations/codex/`
- Modify: `integrations/README.md`, `README.md`, `HANDOFF.md` (only the row that mentions `CLAUDE_TASK_13`), `crates/conch/tests/cli_mcp.rs` (if it references `integrations/`)
- Modify (separate repo): `/Users/ray.hwang/Projects/ofunc/conch-site/site/index.html` install section

- [ ] **Step 1: Confirm nothing in Rust or CI references the deleted files**

Run: `grep -rn "integrations/" --include=*.rs --include=*.yml --include=*.sh . | grep -v target`
Expected: only README/docs hits. If `cli_mcp.rs` or a workflow references `integrations/codex`, update it to point at `skills/join-room/SKILL.md`.

- [ ] **Step 2: Delete and rewrite**

```bash
git rm -r integrations/install.py integrations/validate.py integrations/tests integrations/codex
```

`integrations/README.md` becomes:

```markdown
# Agent integrations

One command per coding agent:

```sh
conch setup claude     # Claude Code
conch setup codex      # OpenAI Codex CLI
conch setup grok       # Grok CLI
conch setup cursor     # Cursor
conch setup gemini     # Gemini CLI
conch setup opencode   # OpenCode
```

`setup` starts `conchd` if needed, writes the `join-room` skill, and merges a `conch` MCP server entry into the host's user config. It never rewrites a file: unrelated keys and comments are preserved, the first edit leaves a `.conch-bak` beside the file, and a file it cannot parse is left alone with the host's own `mcp add` command printed instead.

| host | config | skill |
|---|---|---|
| claude | `~/.claude.json` → `mcpServers.conch` | `~/.claude/skills/join-room/` |
| codex | `~/.codex/config.toml` → `[mcp_servers.conch]` | `~/.agents/skills/join-room/` |
| grok | `~/.grok/config.toml` → `[mcp_servers.conch]` | `~/.agents/skills/join-room/` |
| cursor | `~/.cursor/mcp.json` → `mcpServers.conch` | `~/.agents/skills/join-room/` |
| gemini | `~/.gemini/settings.json` → `mcpServers.conch` | `~/.agents/skills/join-room/` |
| opencode | `~/.config/opencode/opencode.json[c]` → `mcp.conch` | `~/.config/opencode/skills/join-room/` |

Options: `--agent ID` (default `agent:<host>`), `--scope project` for the project-level file in the current directory, `--env K=V` to add environment (for example `CONCH_NODE`), `--dry-run` to print the diff.

Run `conch doctor` to see which hosts are configured and whether their skill copy is current.
```

`README.md`: insert after the title paragraph, before `## Workspace`:

```markdown
## Quickstart

```sh
brew tap OriginalFunction/tap && brew install OriginalFunction/tap/conch   # macOS
# Linux: curl -fsSLo /tmp/conch-install https://conch.originalfunction.com/install.sh && bash /tmp/conch-install

conch setup claude          # or codex, grok, cursor, gemini, opencode — starts conchd for you
conch create --name "My first room"
open http://127.0.0.1:7420/  # the room console
```

`conch doctor` explains the installation; `conch up --service` keeps the daemon running across reboots.
```

Replace the `## Agent integrations` section body with a one-paragraph pointer to `integrations/README.md` and the six `conch setup` names; delete the `cargo install` / `python3 integrations/install.py` lines. In the `## Install` section's launchd/systemd sentence mention `conch up --service`.

`HANDOFF.md`: delete the sentence referring to `CLAUDE_TASK_13_AGENT_INTEGRATIONS.md` validators if present.

- [ ] **Step 3: Update the product site install section (other repo, no deploy)**

In `/Users/ray.hwang/Projects/ofunc/conch-site/site/index.html`, in the `id="install"` section, after the macOS `brew install` command replace the `brew services start conch` step and the `conch create` step with:

```
conch setup claude          # starts conchd, wires Claude Code (or codex, grok, cursor, gemini, opencode)
conch create --name "My first room"
```

and for Linux replace `~/.local/bin/conchd --localhost` with `~/.local/bin/conch setup claude`. Keep the copy buttons' `data-copy` attributes in sync with the visible text. Run `npm test` in that repo. Commit there with `docs: install section uses conch setup`. **Do not run `npm run deploy`** — ask before deploying.

- [ ] **Step 4: Full workspace verification**

Run:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
bash scripts/check-packaging.sh
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add -A integrations README.md HANDOFF.md crates
git commit -m "chore: replace Python integrations with conch setup; add Quickstart"
```

---

## Self-review

**Spec coverage**

| Spec section | Task |
|---|---|
| Command surface (`setup`, `up`, `down`, `doctor`, flags, defaults) | 5, 6, 8 |
| Host table, skill dirs, `$CODEX_HOME`/`$GROK_HOME`/`$OPENCODE_CONFIG` | 3 |
| File editing rules (merge, refuse on parse error + fallback, `.conch-bak` once, no-op rerun, skill version header) | 4, 5 |
| Post-setup message | 3 (`next_step`), 5 |
| Spawn path, pid file, log, port wait, refuse when live | 1, 2, 6 |
| Auto-spawn rules (default node only, refused only, once, stderr line, MCP) | 6 |
| Services (launchd/systemd/Homebrew), packaged units fixed | 7 |
| `doctor` checks 1–7, exit code | 8 |
| `conchd` `version` request | 2 |
| Error remediation table | 6 |
| Removals, migration note for legacy Codex plugin | 8 (doctor warning), 9 |
| Documentation (README Quickstart, integrations README, site, help examples) | 5–9 |
| Testing matrix | 1, 2, 4, 5, 6, 7, 8 |

Gap noted and closed: the spec's `--env` for TOML hosts uses a sub-table for Codex and inline for Grok — Task 5 selects `EnvStyle` by host. The spec's "free space" item in doctor check 5 is intentionally reduced to path + mode; free space adds a platform-specific dependency for little value — record this as a deliberate omission when reviewing.

**Type consistency**: `SpawnOptions`, `PidFile`, `Host`, `Scope`, `Env`, `SetupOptions`, `SetupReport`, `DaemonProbe`, `Check` are used with the same field names across Tasks 1–8; `conch_mcp::run` gains one trailing `bool`.

**Placeholders**: none; the only "adjust until matches" instructions are for whitespace in exact-string tests, with the expected strings given.

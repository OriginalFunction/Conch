//! `conch doctor`: one line per check, remedies inline, exit 1 on any failure.

use std::{
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crate::{
    edit,
    hosts::{Format, Host, Scope, ALL_HOSTS},
    service, setup,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
    Skip,
}

pub struct Check {
    pub level: Level,
    pub name: &'static str,
    pub detail: String,
    pub remedy: Option<String>,
}

pub struct DoctorInput {
    pub cli_version: String,
    /// `None` when the running binary's own path could not be determined.
    pub conch_binary: Option<PathBuf>,
    pub daemon: DaemonProbe,
    pub data_dir: PathBuf,
    /// `None` when `HOME` is not set.
    pub home: Option<PathBuf>,
    pub current_room: Option<CurrentRoom>,
}

pub struct CurrentRoom {
    pub id: String,
    /// Known only when the daemon answered a `status` request for the room.
    pub name: Option<String>,
    pub head: Option<u64>,
}

pub enum DaemonProbe {
    Unreachable {
        addr: SocketAddr,
    },
    Reachable {
        addr: SocketAddr,
        version: Option<String>,
    },
    /// The configured default node is not an address at all.
    BadAddress {
        node: String,
        error: String,
    },
}

fn check(
    level: Level,
    name: &'static str,
    detail: impl Into<String>,
    remedy: Option<String>,
) -> Check {
    Check {
        level,
        name,
        detail: detail.into(),
        remedy,
    }
}

pub fn run_checks(input: &DoctorInput) -> Vec<Check> {
    let mut out = Vec::new();
    match &input.conch_binary {
        Some(binary) => out.push(check(
            Level::Ok,
            "conch",
            format!("{} version {}", binary.display(), input.cli_version),
            None,
        )),
        None => out.push(check(
            Level::Fail,
            "conch",
            format!(
                "version {} (cannot determine this binary's own path)",
                input.cli_version
            ),
            Some("run conch by its full path, or reinstall it".into()),
        )),
    }

    let duplicates = duplicates_on_path(input.conch_binary.as_deref());
    if !duplicates.is_empty() {
        out.push(check(
            Level::Warn,
            "installs",
            format!("other conch binaries on PATH: {}", duplicates.join(", ")),
            Some("remove the one you do not use so `conch` and `conchd` versions match".into()),
        ));
    }

    match &input.daemon {
        DaemonProbe::BadAddress { node, error } => out.push(check(
            Level::Fail,
            "daemon",
            format!("default node {node} is not an address ({error})"),
            Some("unset CONCH_DEFAULT_TCP, or set it to host:port".into()),
        )),
        DaemonProbe::Unreachable { addr } => out.push(check(
            Level::Fail,
            "daemon",
            format!("not running on {addr}"),
            Some("run `conch up` (or `brew services start conch`)".into()),
        )),
        DaemonProbe::Reachable { addr, version } => {
            match version {
                Some(v) if *v == input.cli_version => out.push(check(
                    Level::Ok,
                    "daemon",
                    format!("reachable on {addr}, version {v}"),
                    None,
                )),
                Some(v) => out.push(check(
                    Level::Warn,
                    "daemon",
                    format!(
                        "reachable on {addr}, version {v} (conch is {})",
                        input.cli_version
                    ),
                    Some("restart it: `conch down && conch up`".into()),
                )),
                None => out.push(check(
                    Level::Warn,
                    "daemon",
                    format!("reachable on {addr}, version unknown (older than 1.3)"),
                    Some("restart it after upgrading: `conch down && conch up`".into()),
                )),
            }
            // A pid file whose pid is dead, or has been recycled by some other
            // program, tells us nothing about how this daemon was started.
            let conchd = conch_launch::PidFile::read(&input.data_dir)
                .filter(|pid| pid.is_alive() && pid.is_conchd());
            let how = match service::unit_state() {
                service::UnitState::Loaded => "service unit loaded",
                service::UnitState::Present => "service unit present but not loaded",
                service::UnitState::Absent => "",
            };
            let how = if !how.is_empty() {
                how
            } else if conchd.is_some()
                && service::is_homebrew(&conch_launch::locate_conchd().ok().unwrap_or_default())
            {
                "Homebrew service"
            } else if conchd.is_some() {
                "started by hand (pid file only)"
            } else {
                "unknown (no pid file)"
            };
            let level = if how.starts_with("started by hand")
                || how.starts_with("unknown")
                || how.ends_with("not loaded")
            {
                Level::Warn
            } else {
                Level::Ok
            };
            out.push(check(
                level,
                "daemon",
                how,
                (level == Level::Warn)
                    .then(|| "it will not survive a reboot; run `conch up --service`".into()),
            ));
        }
    }

    match fs::metadata(&input.data_dir) {
        Ok(meta) => {
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o777
            };
            #[cfg(not(unix))]
            let mode = 0o700;
            let free = free_space(&input.data_dir);
            let free_text = free
                .map(|bytes| format!(" ({} free)", human_bytes(bytes)))
                .unwrap_or_default();
            const LOW_WATER: u64 = 1024 * 1024 * 1024;
            if mode & 0o077 != 0 {
                out.push(check(
                    Level::Warn,
                    "data dir",
                    format!("{} mode {mode:o}{free_text}", input.data_dir.display()),
                    Some(format!("chmod 700 {}", input.data_dir.display())),
                ));
            } else if free.is_some_and(|bytes| bytes < LOW_WATER) {
                out.push(check(
                    Level::Warn,
                    "data dir",
                    format!("{}{free_text}", input.data_dir.display()),
                    Some("free up disk space; every room's ledger and blobs live here".into()),
                ));
            } else {
                out.push(check(
                    Level::Ok,
                    "data dir",
                    format!("{}{free_text}", input.data_dir.display()),
                    None,
                ));
            }
        }
        Err(_) => out.push(check(
            Level::Warn,
            "data dir",
            format!("{} does not exist yet", input.data_dir.display()),
            Some("it is created on first `conch up`".into()),
        )),
    }

    match &input.current_room {
        Some(room) => {
            let mut detail = room.id.clone();
            if let Some(name) = &room.name {
                detail.push_str(&format!(" \"{name}\""));
            }
            if let Some(head) = room.head {
                detail.push_str(&format!(" head {head}"));
            }
            out.push(check(Level::Ok, "current room", detail, None));
        }
        None => out.push(check(
            Level::Skip,
            "current room",
            "none (create or join one)",
            None,
        )),
    }

    match &input.home {
        Some(home) => {
            for host in ALL_HOSTS {
                out.push(host_check(host, home, &input.cli_version));
            }
        }
        None => out.push(check(
            Level::Fail,
            "hosts",
            "HOME is not set, so no host config can be found",
            Some("set HOME to your home directory and run `conch doctor` again".into()),
        )),
    }
    out
}

fn host_check(host: Host, home: &Path, cli_version: &str) -> Check {
    let path = host.config_path(Scope::User, home, Path::new("."));
    let Ok(text) = fs::read_to_string(&path) else {
        return check(Level::Skip, host.name(), "not configured", None);
    };
    let entry = match host.format() {
        Format::Toml => text.parse::<toml_edit::DocumentMut>().ok().and_then(|doc| {
            let server = doc.get("mcp_servers")?.get("conch")?;
            let args = server
                .get("args")?
                .as_array()?
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>();
            let command = server
                .get("command")
                .and_then(|c| c.as_str())
                .map(Into::into);
            Some((agent_from_args(&args)?, command))
        }),
        Format::Json | Format::Jsonc => {
            serde_json::from_str::<serde_json::Value>(&edit::json::strip_comments(&text))
                .ok()
                .and_then(|v| {
                    let entry = v.get(host.key_path()[0])?.get("conch")?;
                    let args: Vec<String> = entry
                        .get("args")
                        .or_else(|| entry.get("command"))
                        .and_then(|a| a.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    // OpenCode's `command` is the whole argv; everyone else's is a string.
                    let command = entry.get("command").and_then(|c| {
                        c.as_str()
                            .map(String::from)
                            .or_else(|| c.as_array()?.first()?.as_str().map(String::from))
                    });
                    Some((agent_from_args(&args)?, command))
                })
        }
    };
    let Some((agent, command)) = entry else {
        let legacy = host == Host::Codex && text.contains("plugins.\"conch@");
        return if legacy {
            check(
                Level::Warn,
                host.name(),
                "legacy Codex plugin entry only",
                Some("run `conch setup codex` to migrate".into()),
            )
        } else {
            check(Level::Skip, host.name(), "not configured", None)
        };
    };
    // An entry that names a binary which is no longer there is worse than no entry:
    // the host reports a broken MCP server instead of an unconfigured one.
    let path = env::var_os("PATH");
    if let Some(command) = command.filter(|command| !command_exists(command, path.as_deref())) {
        return check(
            Level::Warn,
            host.name(),
            format!("{agent}, command {command} missing"),
            Some(format!("run `conch setup {}`", host.name())),
        );
    }
    let skill = fs::read_to_string(host.skill_dir(home).join("SKILL.md")).ok();
    let detail = match skill.as_deref() {
        None => format!("{agent}, skill missing"),
        Some(text) => match setup::skill_version(text) {
            Some(v) if v == cli_version => return check(Level::Ok, host.name(), agent, None),
            Some(v) => format!("{agent}, skill {v} is stale"),
            // A copy is there, it just predates the version marker.
            None => format!("{agent}, skill unversioned (pre-1.3)"),
        },
    };
    check(
        Level::Warn,
        host.name(),
        detail,
        Some(format!("run `conch setup {}`", host.name())),
    )
}

/// A command with a path separator is checked where it points; a bare name (`conch`,
/// `npx`) is looked up on PATH the way the host will look it up.
fn command_exists(command: &str, path: Option<&std::ffi::OsStr>) -> bool {
    if command.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(command).exists();
    }
    path.is_some_and(|path| env::split_paths(path).any(|dir| dir.join(command).is_file()))
}

/// Bytes available to this user on the filesystem holding `dir`, via `df -k`, which
/// keeps the crate free of libc. `None` when `df` is unavailable or `dir` is missing.
fn free_space(dir: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg(dir)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_df_available(&String::from_utf8_lossy(&output.stdout))
}

/// The "Available" column of `df -k` output (1K blocks), in bytes.
fn parse_df_available(output: &str) -> Option<u64> {
    let header = output.lines().next()?;
    let column = header
        .split_whitespace()
        .position(|field| field.starts_with("Avail"))?;
    // One path was asked about, so everything after the header is that row — even
    // when df wraps a long device name onto a line of its own.
    let blocks: u64 = output
        .lines()
        .skip(1)
        .flat_map(str::split_whitespace)
        .nth(column)?
        .parse()
        .ok()?;
    Some(blocks * 1024)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn agent_from_args(args: &[String]) -> Option<String> {
    let index = args.iter().position(|a| a == "--agent")?;
    args.get(index + 1).cloned()
}

/// Other `conch` binaries on PATH, each named once however many times PATH repeats
/// the directory it sits in.
fn duplicates_on_path(current: Option<&Path>) -> Vec<String> {
    let canonical = current.and_then(|current| fs::canonicalize(current).ok());
    let Some(path) = env::var_os("PATH") else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for candidate in env::split_paths(&path).map(|dir| dir.join("conch")) {
        if !candidate.is_file() {
            continue;
        }
        let resolved = fs::canonicalize(&candidate).ok();
        if resolved.is_some() && resolved == canonical {
            continue;
        }
        if seen.insert(resolved.unwrap_or_else(|| candidate.clone())) {
            out.push(candidate.display().to_string());
        }
    }
    out
}

pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        let tag = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Skip => "--  ",
        };
        out.push_str(&format!("{tag}  {:<13} {}\n", c.name, c.detail));
        if let Some(remedy) = &c.remedy {
            out.push_str(&format!("      {:<13} \u{2192} {remedy}\n", ""));
        }
    }
    out
}

pub fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.level == Level::Fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn df_output_yields_the_available_column_in_bytes() {
        let macos = "Filesystem   1024-blocks       Used Available Capacity  iused      ifree %iused  Mounted on\n\
                     /dev/disk3s5  1948455240 1764976088 137322744    93% 16685179 1373227440    1%   /System/Volumes/Data\n";
        assert_eq!(parse_df_available(macos), Some(137_322_744 * 1024));
        let linux = "Filesystem     1K-blocks      Used Available Use% Mounted on\n\
                     /dev/nvme0n1p2 490691512 300000000 165000000  65% /\n";
        assert_eq!(parse_df_available(linux), Some(165_000_000 * 1024));
        // A long device name makes df wrap the row: the filesystem sits on its own
        // line and the numbers follow on the next.
        let wrapped = "Filesystem                          1K-blocks      Used Available Use% Mounted on\n\
                       fileserver.example.com:/export/home/very/long/path\n\
                                                           990000000 500000000 480000000  52% /home/u\n";
        assert_eq!(parse_df_available(wrapped), Some(480_000_000 * 1024));
        assert_eq!(
            parse_df_available("df: /nope: No such file or directory\n"),
            None
        );
        assert_eq!(parse_df_available(""), None);
    }

    #[test]
    fn free_space_is_rendered_in_human_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(140_618_489_856), "131.0 GB");
        assert_eq!(human_bytes(1_500_000), "1.4 MB");
    }

    #[test]
    fn a_bare_command_name_is_looked_up_on_path_and_a_path_is_checked_directly() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = dir.path().join("tool");
        fs::write(&tool, "").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert!(command_exists("tool", Some(&path)));
        assert!(!command_exists("other", Some(&path)));
        assert!(!command_exists("tool", None));
        assert!(command_exists(&tool.display().to_string(), None));
        assert!(!command_exists("/nonexistent/tool", Some(&path)));
    }
}

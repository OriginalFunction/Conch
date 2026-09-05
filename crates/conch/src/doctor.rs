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
    pub current_room: Option<String>,
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
            let how = if service::unit_installed() {
                "service unit installed"
            } else if conchd.is_some()
                && service::is_homebrew(&conch_launch::locate_conchd().ok().unwrap_or_default())
            {
                "Homebrew service"
            } else if conchd.is_some() {
                "started by hand (pid file only)"
            } else {
                "unknown (no pid file)"
            };
            let level = if how.starts_with("started by hand") || how.starts_with("unknown") {
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
            if mode & 0o077 != 0 {
                out.push(check(
                    Level::Warn,
                    "data dir",
                    format!("{} mode {mode:o}", input.data_dir.display()),
                    Some(format!("chmod 700 {}", input.data_dir.display())),
                ));
            } else {
                out.push(check(
                    Level::Ok,
                    "data dir",
                    input.data_dir.display().to_string(),
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
        Some(room) => out.push(check(Level::Ok, "current room", room.clone(), None)),
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
    if let Some(command) = command.filter(|command| !Path::new(command).exists()) {
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

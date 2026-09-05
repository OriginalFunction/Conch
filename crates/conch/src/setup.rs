//! `conch setup <host>`: write the join-room skill and merge the MCP entry.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use crate::{
    edit,
    hosts::{Env, Format, Host, Scope},
};

const SKILL_SOURCE: &str = include_str!("../../../skills/join-room/SKILL.md");
const VERSION_MARK: &str = "<!-- conch-skill-version: ";

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("{path} could not be parsed ({source}); nothing was written.\nRegister the server yourself:\n{fallback}")]
    Unparseable {
        path: PathBuf,
        source: Box<edit::EditError>,
        fallback: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub struct SetupOptions {
    pub host: Host,
    pub agent: String,
    pub scope: Scope,
    pub env: Env,
    pub dry_run: bool,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub conch_binary: PathBuf,
    pub version: String,
}

pub struct SetupReport {
    pub config_path: PathBuf,
    pub config_changed: bool,
    pub skill_path: PathBuf,
    pub skill_changed: bool,
    pub backup_path: Option<PathBuf>,
    pub diff: String,
    pub next_step: &'static str,
}

/// Embedded skill with a version comment placed right after the YAML frontmatter.
pub fn skill_text(version: &str) -> String {
    let end = SKILL_SOURCE
        .match_indices("\n---\n")
        .next()
        .map(|(i, _)| i + 5)
        .unwrap_or(0);
    format!(
        "{}{VERSION_MARK}{version} -->\n{}",
        &SKILL_SOURCE[..end],
        &SKILL_SOURCE[end..]
    )
}

pub fn skill_version(text: &str) -> Option<String> {
    let start = text.find(VERSION_MARK)? + VERSION_MARK.len();
    Some(text[start..].split(" -->").next()?.to_string())
}

fn args_for(agent: &str) -> Vec<String> {
    vec!["--agent".into(), agent.into(), "mcp".into()]
}

/// The path to record in a host config for the running binary.
///
/// `current_exe` resolves symlinks, which turns `/opt/homebrew/bin/conch` into a
/// versioned `/opt/homebrew/Cellar/conch/1.2.2/bin/conch` that the next upgrade
/// deletes. When the name the process was invoked under resolves to the same file,
/// that unresolved path is the stable one and is preferred. A relative invocation
/// (`./target/debug/conch`) is anchored to `cwd` so the recorded path is always
/// absolute; without a `cwd` it falls back to the resolved executable.
pub fn stable_binary_path_from(
    argv0: Option<&std::ffi::OsStr>,
    current_exe: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    let exe = current_exe?;
    let canonical = fs::canonicalize(exe).ok();
    if let (Some(argv0), Some(canonical)) = (argv0, canonical.as_deref()) {
        let argv0 = Path::new(argv0);
        let candidates: Vec<PathBuf> = if argv0.is_absolute() {
            vec![argv0.to_path_buf()]
        } else if argv0.components().count() > 1 {
            // `./bin/conch` → `<cwd>/bin/conch`: anchor it, dropping the `.` segments.
            cwd.map(|cwd| {
                let mut anchored = cwd.to_path_buf();
                anchored.extend(
                    argv0
                        .components()
                        .filter(|component| !matches!(component, std::path::Component::CurDir)),
                );
                vec![anchored]
            })
            .unwrap_or_default()
        } else {
            path.map(|path| {
                std::env::split_paths(path)
                    .map(|dir| dir.join(argv0))
                    .collect()
            })
            .unwrap_or_default()
        };
        for candidate in candidates {
            if candidate.is_file()
                && fs::canonicalize(&candidate).ok().as_deref() == Some(canonical)
            {
                return Some(candidate);
            }
        }
    }
    Some(exe.to_path_buf())
}

pub fn run(options: &SetupOptions) -> Result<SetupReport, SetupError> {
    let host = options.host;
    let command = options.conch_binary.display().to_string();
    let args = args_for(&options.agent);
    let config_path = host.config_path(options.scope, &options.home, &options.cwd);
    let existing = match fs::read_to_string(&config_path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let before = existing.clone().unwrap_or_default();
    let unparseable = |source: edit::EditError| SetupError::Unparseable {
        path: config_path.clone(),
        source: Box::new(source),
        fallback: host.fallback_command(&command, &args),
    };
    // Refuse a file we cannot understand before touching anything, and refuse our own
    // result if the merge produced something that would not parse: the spec is that a
    // file which fails to parse is never written.
    if !before.trim().is_empty() {
        validate(host.format(), &before).map_err(&unparseable)?;
    }
    let merged = match host.format() {
        Format::Json | Format::Jsonc => edit::json::set_member(
            &before,
            host.key_path(),
            &host.render_json_entry(&command, &args, &options.env),
        ),
        Format::Toml => edit::toml::set_server(
            &before,
            host.key_path()[0],
            host.key_path()[1],
            &edit::toml::Server {
                command: &command,
                args: &args,
                env: &options.env,
                env_style: if host == Host::Grok {
                    edit::toml::EnvStyle::Inline
                } else {
                    edit::toml::EnvStyle::SubTable
                },
            },
        ),
    }
    .map_err(&unparseable)?;
    validate(host.format(), &merged).map_err(&unparseable)?;
    let config_changed = merged != before;
    // Only `--dry-run` ever prints the diff; computing it otherwise is pure cost.
    let diff = if options.dry_run {
        similar::TextDiff::from_lines(&before, &merged)
            .unified_diff()
            .context_radius(2)
            .header(
                &config_path.display().to_string(),
                &config_path.display().to_string(),
            )
            .to_string()
    } else {
        String::new()
    };

    let skill_path = host.skill_dir(&options.home).join("SKILL.md");
    let wanted_skill = skill_text(&options.version);
    let skill_changed = fs::read_to_string(&skill_path)
        .ok()
        .and_then(|t| skill_version(&t))
        != Some(options.version.clone());

    // Host configs hold credentials. Whatever mode the file already had is the mode
    // it keeps, in the rewrite and in the backup beside it.
    let config_mode = file_mode(&config_path);
    let mut backup_path = None;
    if !options.dry_run {
        if config_changed {
            if let Some(original) = &existing {
                let backup = config_path.with_file_name(format!(
                    "{}.conch-bak",
                    config_path.file_name().unwrap().to_string_lossy()
                ));
                if !backup.exists() {
                    write_atomic(&backup, original, config_mode)?;
                    backup_path = Some(backup);
                }
            }
            if let Some(parent) = config_path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_atomic(&config_path, &merged, config_mode)?;
        }
        if skill_changed {
            fs::create_dir_all(skill_path.parent().unwrap())?;
            let skill_mode = file_mode(&skill_path);
            write_atomic(&skill_path, &wanted_skill, skill_mode)?;
        }
    }
    Ok(SetupReport {
        config_path,
        config_changed,
        skill_path,
        skill_changed,
        backup_path,
        diff,
        next_step: host.next_step(),
    })
}

/// Does this text still parse as the host's format? JSONC is reduced to plain JSON
/// first, so comments and trailing commas are not mistaken for damage.
fn validate(format: Format, text: &str) -> Result<(), edit::EditError> {
    match format {
        Format::Json => serde_json::from_str::<serde_json::Value>(text)
            .map(|_| ())
            .map_err(|error| edit::EditError::Json(error.to_string())),
        Format::Jsonc => {
            let plain = strip_trailing_commas(&edit::json::strip_comments(text));
            serde_json::from_str::<serde_json::Value>(&plain)
                .map(|_| ())
                .map_err(|error| edit::EditError::Json(error.to_string()))
        }
        Format::Toml => text
            .parse::<toml_edit::DocumentMut>()
            .map(|_| ())
            .map_err(edit::EditError::Toml),
    }
}

/// Drop each `,` that sits directly before a closing `}` or `]` so JSONC passes a
/// strict JSON parse. Comments must already have been removed.
fn strip_trailing_commas(text: &str) -> String {
    let mut marked: Vec<(char, bool)> = Vec::new();
    let (mut in_string, mut escaped) = (false, false);
    for ch in text.chars() {
        let was_in_string = in_string;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
        }
        marked.push((ch, was_in_string));
    }
    let mut out = String::with_capacity(text.len());
    for (index, &(ch, inside)) in marked.iter().enumerate() {
        if !inside && ch == ',' {
            let next = marked[index + 1..]
                .iter()
                .find(|(c, inside)| *inside || !c.is_whitespace());
            if matches!(next, Some((c, false)) if *c == '}' || *c == ']') {
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// The existing file's permission bits, or `None` when it does not exist yet (in which
/// case the process umask decides, as for any newly created file).
#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Option<u32> {
    None
}

/// Replace `path` in one step. The temporary is created with `mode` from the outset so
/// its contents are never briefly readable by anyone the original excluded.
fn write_atomic(path: &Path, text: &str, mode: Option<u32>) -> std::io::Result<()> {
    let tmp = path.with_extension("conch-tmp");
    let _ = fs::remove_file(&tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    // `create_new(true)` applies the umask on top of `mode`; restore the exact bits.
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_path_entry_that_points_at_this_binary_beats_the_resolved_one() {
        let exe = std::env::current_exe().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        // Stands in for /opt/homebrew/bin/conch -> ../Cellar/conch/X.Y.Z/bin/conch.
        let link = dir.path().join("conch");
        std::os::unix::fs::symlink(&exe, &link).unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();

        assert_eq!(
            stable_binary_path_from(
                Some(std::ffi::OsStr::new("conch")),
                Some(&exe),
                Some(&path),
                None
            ),
            Some(link)
        );
        // Nothing on PATH resolves to it: keep the resolved path.
        let empty = tempfile::TempDir::new().unwrap();
        let empty_path = std::env::join_paths([empty.path()]).unwrap();
        assert_eq!(
            stable_binary_path_from(
                Some(std::ffi::OsStr::new("conch")),
                Some(&exe),
                Some(&empty_path),
                None
            ),
            Some(exe.clone())
        );
        assert_eq!(stable_binary_path_from(None, None, None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_invocation_is_recorded_as_an_absolute_path() {
        let exe = std::env::current_exe().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        let link = dir.path().join("bin/conch");
        std::os::unix::fs::symlink(&exe, &link).unwrap();

        // Invoked as `./bin/conch` from `dir`: keep the symlink, drop the relativity.
        assert_eq!(
            stable_binary_path_from(
                Some(std::ffi::OsStr::new("./bin/conch")),
                Some(&exe),
                None,
                Some(dir.path())
            ),
            Some(link.clone())
        );
        // Absolute invocations are kept verbatim.
        assert_eq!(
            stable_binary_path_from(Some(link.as_os_str()), Some(&exe), None, None),
            Some(link)
        );
        // A relative name with no cwd to anchor it falls back to the resolved path.
        assert_eq!(
            stable_binary_path_from(
                Some(std::ffi::OsStr::new("./bin/conch")),
                Some(&exe),
                None,
                None
            ),
            Some(exe)
        );
    }

    #[test]
    fn version_header_sits_after_frontmatter() {
        let text = skill_text("9.9.9");
        let fence_end = text.find("\n---\n").unwrap() + 5;
        assert!(
            text[fence_end..].starts_with("<!-- conch-skill-version: 9.9.9 -->\n\n# Join"),
            "{}",
            &text[fence_end..fence_end + 60]
        );
        assert_eq!(skill_version(&text).as_deref(), Some("9.9.9"));
    }
}

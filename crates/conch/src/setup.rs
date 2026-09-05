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
    let diff = similar::TextDiff::from_lines(&before, &merged)
        .unified_diff()
        .context_radius(2)
        .header(
            &config_path.display().to_string(),
            &config_path.display().to_string(),
        )
        .to_string();

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

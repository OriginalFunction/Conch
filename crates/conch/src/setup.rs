//! `conch setup <host>`: write the join-room skill and merge the MCP entry.

use std::{
    fs,
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
    .map_err(|source| SetupError::Unparseable {
        path: config_path.clone(),
        source: Box::new(source),
        fallback: host.fallback_command(&command, &args),
    })?;
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

    let mut backup_path = None;
    if !options.dry_run {
        if config_changed {
            if let Some(original) = &existing {
                let backup = config_path.with_file_name(format!(
                    "{}.conch-bak",
                    config_path.file_name().unwrap().to_string_lossy()
                ));
                if !backup.exists() {
                    fs::write(&backup, original)?;
                    backup_path = Some(backup);
                }
            }
            if let Some(parent) = config_path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_atomic(&config_path, &merged)?;
        }
        if skill_changed {
            fs::create_dir_all(skill_path.parent().unwrap())?;
            write_atomic(&skill_path, &wanted_skill)?;
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

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("conch-tmp");
    fs::write(&tmp, text)?;
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

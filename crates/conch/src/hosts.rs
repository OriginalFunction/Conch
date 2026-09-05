//! Per-host data for `conch setup`: where the MCP config lives, its format, and the entry shape.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Claude,
    Codex,
    Grok,
    Cursor,
    Gemini,
    Opencode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Jsonc,
    Toml,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env(pub Vec<(String, String)>);

pub const ALL_HOSTS: [Host; 6] = [
    Host::Claude,
    Host::Codex,
    Host::Grok,
    Host::Cursor,
    Host::Gemini,
    Host::Opencode,
];

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialises")
}

fn json_array(items: impl Iterator<Item = String>) -> String {
    format!(
        "[{}]",
        items.map(|s| json_string(&s)).collect::<Vec<_>>().join(",")
    )
}

fn json_env(env: &Env) -> String {
    format!(
        "{{{}}}",
        env.0
            .iter()
            .map(|(k, v)| format!("{}:{}", json_string(k), json_string(v)))
            .collect::<Vec<_>>()
            .join(",")
    )
}

impl Host {
    pub fn parse(name: &str) -> Option<Host> {
        ALL_HOSTS.into_iter().find(|host| host.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Host::Claude => "claude",
            Host::Codex => "codex",
            Host::Grok => "grok",
            Host::Cursor => "cursor",
            Host::Gemini => "gemini",
            Host::Opencode => "opencode",
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
        let env_dir = |var: &str, default: PathBuf| {
            std::env::var_os(var).map(PathBuf::from).unwrap_or(default)
        };
        match (self, scope) {
            (Host::Claude, Scope::User) => home.join(".claude.json"),
            (Host::Claude, Scope::Project) => cwd.join(".mcp.json"),
            (Host::Codex, Scope::User) => {
                env_dir("CODEX_HOME", home.join(".codex")).join("config.toml")
            }
            (Host::Codex, Scope::Project) => cwd.join(".codex/config.toml"),
            (Host::Grok, Scope::User) => {
                env_dir("GROK_HOME", home.join(".grok")).join("config.toml")
            }
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
                if jsonc.is_file() {
                    jsonc
                } else {
                    dir.join("opencode.json")
                }
            }
            (Host::Opencode, Scope::Project) => {
                let jsonc = cwd.join("opencode.jsonc");
                if jsonc.is_file() {
                    jsonc
                } else {
                    cwd.join("opencode.json")
                }
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
            let key = if self == Host::Opencode {
                "environment"
            } else {
                "env"
            };
            out.push_str(&format!(r#","{key}":{}"#, json_env(env)));
        }
        out.push('}');
        out
    }

    pub fn skill_dir(self, home: &Path) -> PathBuf {
        match self {
            Host::Claude => home.join(".claude/skills/join-room"),
            Host::Opencode => home.join(".config/opencode/skills/join-room"),
            Host::Codex | Host::Grok | Host::Cursor | Host::Gemini => {
                home.join(".agents/skills/join-room")
            }
        }
    }

    pub fn next_step(self) -> &'static str {
        match self {
            Host::Claude => {
                "Restart Claude Code, then ask it to join a ticket: `join ./my-room.conch`."
            }
            Host::Codex => "Start a new Codex thread so it discovers the conch server and skill.",
            Host::Grok => "Start a new Grok session; `grok mcp list` shows conch.",
            Host::Cursor => {
                "Reload the Cursor window and confirm conch appears under Settings → MCP."
            }
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
                self.config_path(Scope::User, Path::new("~"), Path::new("."))
                    .display(),
                self.render_json_entry(command, args, &Env::default())
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_paths_follow_the_spec_table() {
        let home = Path::new("/h");
        let cwd = Path::new("/p");
        assert_eq!(
            Host::Claude.config_path(Scope::User, home, cwd),
            Path::new("/h/.claude.json")
        );
        assert_eq!(
            Host::Codex.config_path(Scope::User, home, cwd),
            Path::new("/h/.codex/config.toml")
        );
        assert_eq!(
            Host::Grok.config_path(Scope::User, home, cwd),
            Path::new("/h/.grok/config.toml")
        );
        assert_eq!(
            Host::Cursor.config_path(Scope::User, home, cwd),
            Path::new("/h/.cursor/mcp.json")
        );
        assert_eq!(
            Host::Gemini.config_path(Scope::User, home, cwd),
            Path::new("/h/.gemini/settings.json")
        );
        assert_eq!(
            Host::Opencode.config_path(Scope::User, home, cwd),
            Path::new("/h/.config/opencode/opencode.json")
        );
        assert_eq!(
            Host::Claude.config_path(Scope::Project, home, cwd),
            Path::new("/p/.mcp.json")
        );
        assert_eq!(
            Host::Opencode.config_path(Scope::Project, home, cwd),
            Path::new("/p/opencode.json")
        );
    }

    #[test]
    fn skill_dirs_share_agents_for_four_hosts() {
        let home = Path::new("/h");
        assert_eq!(
            Host::Claude.skill_dir(home),
            Path::new("/h/.claude/skills/join-room")
        );
        for host in [Host::Codex, Host::Grok, Host::Cursor, Host::Gemini] {
            assert_eq!(
                host.skill_dir(home),
                Path::new("/h/.agents/skills/join-room")
            );
        }
        assert_eq!(
            Host::Opencode.skill_dir(home),
            Path::new("/h/.config/opencode/skills/join-room")
        );
    }

    #[test]
    fn json_entries_match_each_host_shape() {
        let args = vec![
            "--agent".to_string(),
            "agent:x".to_string(),
            "mcp".to_string(),
        ];
        let env = Env(vec![]);
        assert_eq!(
            Host::Claude.render_json_entry("/b/conch", &args, &env),
            r#"{"type":"stdio","command":"/b/conch","args":["--agent","agent:x","mcp"]}"#
        );
        assert_eq!(
            Host::Cursor.render_json_entry("/b/conch", &args, &env),
            r#"{"command":"/b/conch","args":["--agent","agent:x","mcp"]}"#
        );
        assert_eq!(
            Host::Opencode.render_json_entry("/b/conch", &args, &env),
            r#"{"type":"local","command":["/b/conch","--agent","agent:x","mcp"],"enabled":true}"#
        );
        let env = Env(vec![("CONCH_NODE".into(), "tcp://127.0.0.1:9".into())]);
        assert_eq!(
            Host::Gemini.render_json_entry("/b/conch", &args, &env),
            r#"{"command":"/b/conch","args":["--agent","agent:x","mcp"],"env":{"CONCH_NODE":"tcp://127.0.0.1:9"}}"#
        );
        assert_eq!(
            Host::Opencode.render_json_entry("/b/conch", &args, &env),
            r#"{"type":"local","command":["/b/conch","--agent","agent:x","mcp"],"enabled":true,"environment":{"CONCH_NODE":"tcp://127.0.0.1:9"}}"#
        );
    }

    #[test]
    fn default_agent_and_parse() {
        assert_eq!(Host::parse("cursor"), Some(Host::Cursor));
        assert_eq!(Host::parse("vim"), None);
        assert_eq!(Host::Gemini.default_agent(), "agent:gemini");
    }
}

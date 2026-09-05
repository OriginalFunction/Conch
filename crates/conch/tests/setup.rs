use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

fn conch(home: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(args)
        .env("HOME", home)
        .env("CONCH_SETUP_SKIP_DAEMON", "1")
        .env_remove("CODEX_HOME")
        .env_remove("GROK_HOME")
        .env_remove("OPENCODE_CONFIG")
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn ok(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    assert_eq!(
        entry["args"],
        serde_json::json!(["--agent", "agent:claude", "mcp"])
    );
    let skill = fs::read_to_string(home.path().join(".claude/skills/join-room/SKILL.md")).unwrap();
    assert!(skill.starts_with("---\nname: join-room\n"));
    assert!(skill.contains(&format!(
        "<!-- conch-skill-version: {} -->",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(out.contains("Restart Claude Code"), "{out}");
    assert!(
        !home.path().join(".claude.json.conch-bak").exists(),
        "no backup for a freshly created file"
    );
}

#[test]
fn codex_setup_preserves_comments_and_other_servers_and_backs_up_once() {
    let home = TempDir::new().unwrap();
    let codex = home.path().join(".codex");
    fs::create_dir_all(&codex).unwrap();
    let original = "# mine\nmodel = \"gpt\"\n\n[mcp_servers.pencil]\ncommand = \"p\"\n";
    fs::write(codex.join("config.toml"), original).unwrap();
    ok(&conch(
        home.path(),
        home.path(),
        &["setup", "codex", "--agent", "agent:codex-2"],
    ));
    let config = fs::read_to_string(codex.join("config.toml")).unwrap();
    assert!(config.starts_with("# mine\nmodel = \"gpt\"\n"));
    assert!(config.contains("[mcp_servers.pencil]\ncommand = \"p\"\n"));
    assert!(config.contains("[mcp_servers.conch]\n"));
    assert!(config.contains("args = [\"--agent\", \"agent:codex-2\", \"mcp\"]"));
    assert_eq!(
        fs::read_to_string(codex.join("config.toml.conch-bak")).unwrap(),
        original
    );
    assert!(home
        .path()
        .join(".agents/skills/join-room/SKILL.md")
        .is_file());

    // second run: no change, backup untouched
    let out = ok(&conch(
        home.path(),
        home.path(),
        &["setup", "codex", "--agent", "agent:codex-2"],
    ));
    assert!(out.contains("already configured"), "{out}");
    assert_eq!(
        fs::read_to_string(codex.join("config.toml.conch-bak")).unwrap(),
        original
    );
}

#[test]
fn opencode_jsonc_keeps_comments_and_env_lands_in_environment() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join(".config/opencode");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("opencode.jsonc"), "{\n  // hi\n  \"$schema\": \"s\",\n  \"mcp\": {\n    \"pencil\": { \"type\": \"local\", \"command\": [\"p\"] }\n  }\n}\n").unwrap();
    ok(&conch(
        home.path(),
        home.path(),
        &["setup", "opencode", "--env", "CONCH_NODE=tcp://127.0.0.1:9"],
    ));
    let config = fs::read_to_string(dir.join("opencode.jsonc")).unwrap();
    assert!(config.contains("// hi"));
    assert!(config.contains("\"pencil\": { \"type\": \"local\", \"command\": [\"p\"] }"));
    assert!(
        config
            .contains("\"environment\": {\n        \"CONCH_NODE\": \"tcp://127.0.0.1:9\"\n      }"),
        "{config}"
    );
    assert!(!dir.join("opencode.json").exists());
}

#[test]
fn malformed_config_is_refused_and_fallback_printed() {
    let home = TempDir::new().unwrap();
    fs::write(home.path().join(".claude.json"), "{ \"mcpServers\": ").unwrap();
    let output = conch(home.path(), home.path(), &["setup", "claude"]);
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("claude mcp add --scope user conch --"),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(home.path().join(".claude.json")).unwrap(),
        "{ \"mcpServers\": "
    );
}

#[test]
fn dry_run_writes_nothing_and_shows_a_diff() {
    let home = TempDir::new().unwrap();
    let out = ok(&conch(
        home.path(),
        home.path(),
        &["setup", "cursor", "--dry-run"],
    ));
    assert!(out.contains("+  \"mcpServers\""), "{out}");
    assert!(!home.path().join(".cursor").exists());
}

#[test]
fn project_scope_targets_cwd_files() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    ok(&conch(
        home.path(),
        project.path(),
        &["setup", "gemini", "--scope", "project"],
    ));
    assert!(project.path().join(".gemini/settings.json").is_file());
    assert!(!home.path().join(".gemini").exists());
}

#[test]
fn unknown_host_is_rejected() {
    let home = TempDir::new().unwrap();
    let output = conch(home.path(), home.path(), &["setup", "vim"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("claude, codex, grok, cursor, gemini, opencode"));
}

use std::{
    fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use conch_launch::PidFile;
use tempfile::TempDir;

/// conchd built by the workspace; fall back to building it so `cargo test -p conch` works alone.
fn conchd_binary() -> PathBuf {
    let sibling = PathBuf::from(env!("CARGO_BIN_EXE_conch")).with_file_name("conchd");
    if !sibling.is_file() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "conchd", "--bin", "conchd"])
            .status()
            .unwrap();
        assert!(status.success());
    }
    sibling
}

/// Two loopback ports that stay reserved until the caller drops the listeners, closing
/// the window in which another test (or anything else on the machine) could take them.
fn reserve_ports() -> (SocketAddr, SocketAddr, (TcpListener, TcpListener)) {
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    let addrs = (tcp.local_addr().unwrap(), http.local_addr().unwrap());
    (addrs.0, addrs.1, (tcp, http))
}

/// Stops whatever the pid file names when the test ends, so a failing assertion
/// never leaks a daemon.
struct DaemonGuard(PathBuf);

impl DaemonGuard {
    fn new(data_dir: &Path) -> Self {
        Self(data_dir.to_path_buf())
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = PidFile::read(&self.0) {
            let _ = pid.stop(Duration::from_secs(5));
        }
    }
}

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

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[test]
fn setup_keeps_the_configs_own_mode_and_writes_the_backup_the_same_way() {
    use std::os::unix::fs::PermissionsExt;
    let home = TempDir::new().unwrap();
    let config = home.path().join(".claude.json");
    // Host configs hold API credentials; people lock them down and setup must not
    // hand them back to the rest of the machine.
    fs::write(&config, "{\n  \"other\": 1\n}\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

    ok(&conch(home.path(), home.path(), &["setup", "claude"]));

    assert_eq!(mode_of(&config), 0o600, "config mode preserved");
    let backup = home.path().join(".claude.json.conch-bak");
    assert!(backup.is_file());
    assert_eq!(mode_of(&backup), 0o600, "backup is not world-readable");
    assert!(!home.path().join(".claude.conch-tmp").exists());
}

#[test]
fn a_config_that_would_not_parse_afterwards_is_never_written() {
    let home = TempDir::new().unwrap();
    let config = home.path().join(".claude.json");
    // Scans far enough for the member insert to succeed, but the result is not JSON.
    let original = "{\"projects\":{\"a\": ,},\"other\":1}";
    fs::write(&config, original).unwrap();
    let output = conch(home.path(), home.path(), &["setup", "claude"]);
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("could not be parsed"), "{err}");
    assert!(
        err.contains("claude mcp add --scope user conch --"),
        "{err}"
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    assert!(!home.path().join(".claude.json.conch-bak").exists());
    assert!(!home.path().join(".claude.conch-tmp").exists());
}

#[test]
fn dry_run_writes_nothing_and_shows_a_diff() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let (tcp, http, reserved) = reserve_ports();
    let _guard = DaemonGuard::new(data.path());
    drop(reserved);
    // No CONCH_SETUP_SKIP_DAEMON: a dry run must not start a daemon either.
    let output = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["setup", "cursor", "--dry-run"])
        .env("HOME", home.path())
        .env("CONCH_DATA_DIR", data.path())
        .env("CONCH_CONCHD", conchd_binary())
        .env("CONCH_DEFAULT_TCP", tcp.to_string())
        .env("CONCH_DEFAULT_HTTP", http.to_string())
        .env_remove("CONCH_NODE")
        .env_remove("CODEX_HOME")
        .env_remove("GROK_HOME")
        .env_remove("OPENCODE_CONFIG")
        .current_dir(home.path())
        .output()
        .unwrap();
    let out = ok(&output);
    assert!(out.contains("+  \"mcpServers\""), "{out}");
    assert!(!home.path().join(".cursor").exists());
    assert!(
        !data.path().join("conchd.pid").exists(),
        "no daemon spawned"
    );
    assert!(
        !data.path().join("conchd.log").exists(),
        "no daemon spawned"
    );
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
fn skill_only_rerun_does_not_claim_config_was_written() {
    let home = TempDir::new().unwrap();
    ok(&conch(home.path(), home.path(), &["setup", "claude"]));
    let skill_path = home.path().join(".claude/skills/join-room/SKILL.md");
    let skill = fs::read_to_string(&skill_path).unwrap();
    fs::write(
        &skill_path,
        skill.replace(env!("CARGO_PKG_VERSION"), "0.0.1"),
    )
    .unwrap();

    let out = ok(&conch(home.path(), home.path(), &["setup", "claude"]));
    assert!(!out.contains("wrote"), "{out}");
    assert!(out.contains("config already correct"), "{out}");
    assert!(out.contains("skill →"), "{out}");
    let skill = fs::read_to_string(&skill_path).unwrap();
    assert!(skill.contains(&format!(
        "<!-- conch-skill-version: {} -->",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(!home.path().join(".claude.json.conch-bak").exists());
}

#[test]
fn unknown_host_is_rejected() {
    let home = TempDir::new().unwrap();
    let output = conch(home.path(), home.path(), &["setup", "vim"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("claude, codex, grok, cursor, gemini, opencode"));
}

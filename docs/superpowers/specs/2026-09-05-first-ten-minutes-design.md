# First ten minutes: `conch setup`, daemon lifecycle, `conch doctor`

Status: approved design, 2026-09-05. Sub-project 1 of 4 in the turnkey series.

## Goal

A person installs Conch, runs one command per coding agent, and the agent can
join a room on its first MCP call. Nothing in this design touches wrap, floor,
or the ledger format. Spec v1.6 remains law for everything it covers.

## Problem

Today the daemon is never started by any install path, `conch` never starts it,
and the first error a user sees is `Connection refused (os error 61)`. Agent
setup requires a Python script from a repository checkout followed by a
hand-run `mcp add`, supports three hosts, and its tests never run in CI. There
is no command that reports the state of an installation.

## Scope

In: `conch setup <host>`, `conch up`, `conch down`, `conch doctor`, client
auto-spawn of `conchd`, a pid file and `version` request in `conchd`, remedy
lines on common errors, removal of the Python integrations, a user-facing
Quickstart.

Out (later sub-projects): speaker attribution on scenes, mentions, `say`/`tail`,
human-readable output, `conch invite`, tracing, relay.

## Command surface

```
conch setup <claude|codex|grok|cursor|gemini|opencode>
            [--agent ID] [--scope user|project] [--env K=V ...] [--dry-run]
conch up    [--service]
conch down  [--service]
conch doctor
```

### `conch setup`

Runs four steps in order and stops at the first failure:

1. Ensure a daemon is reachable on the default node (spawn one if not; see
   Daemon lifecycle).
2. Write the join-room skill to the host's skill directory.
3. Merge an MCP server entry named `conch` into the host's user-scope config.
4. Print what changed and the host-specific next step.

Defaults: `--agent agent:<host>` (for example `agent:claude`, `agent:cursor`);
`--scope user`. The entry's `command` is the absolute path of the running
`conch` binary. `--env` values are added to the entry's environment field only
when given; the default entry has no environment.

`--dry-run` prints the exact file changes as unified diffs and writes nothing.
`--scope project` writes the project-level file for the host in the current
directory (`.mcp.json`, `.codex/config.toml`, `.grok/config.toml`,
`.cursor/mcp.json`, `.gemini/settings.json`, `opencode.json`).

### Host table

| host | user-scope MCP config | key path | format | entry |
|---|---|---|---|---|
| claude | `~/.claude.json` | `mcpServers.conch` | JSON | `{"type":"stdio","command":C,"args":A,"env":E?}` |
| codex | `~/.codex/config.toml` | `[mcp_servers.conch]` | TOML | `command`, `args`, `[mcp_servers.conch.env]?` |
| grok | `~/.grok/config.toml` | `[mcp_servers.conch]` | TOML | `command`, `args`, `env = {...}?` |
| cursor | `~/.cursor/mcp.json` | `mcpServers.conch` | JSON | `{"command":C,"args":A,"env":E?}` |
| gemini | `~/.gemini/settings.json` | `mcpServers.conch` | JSON | `{"command":C,"args":A,"env":E?}` |
| opencode | `~/.config/opencode/opencode.json` or `.jsonc` | `mcp.conch` | JSONC | `{"type":"local","command":[C,...A],"enabled":true,"environment":E?}` |

`A` is always `["--agent", ID, "mcp"]`. For OpenCode the existing `.jsonc`
file is used when present, otherwise `.json`.

Skill directories:

| host | skill path |
|---|---|
| claude | `~/.claude/skills/join-room/SKILL.md` |
| codex, grok, cursor, gemini | `~/.agents/skills/join-room/SKILL.md` |
| opencode | `~/.config/opencode/skills/join-room/SKILL.md` |

`$CODEX_HOME`, `$GROK_HOME`, and `$OPENCODE_CONFIG` are honoured when set. All
paths derive from `$HOME`; tests set `HOME` to a temporary directory.

### File editing rules

- Merge, never rewrite. JSON files are parsed, the single key path is set, and
  the document is re-serialised with the file's existing indentation; all other
  keys are preserved. TOML files are edited with `toml_edit` so comments and
  layout survive. JSONC files are edited by locating the `mcp` object and
  inserting or replacing the `conch` member textually; comments elsewhere are
  untouched. A missing file is created containing only the entry.
- A file that fails to parse is never written. `setup` reports the parse error
  and prints the host's own registration command (`claude mcp add ...`,
  `codex mcp add ...`, `grok mcp add ...`, `gemini mcp add ...`) or, for Cursor
  and OpenCode, the exact JSON to paste.
- The first write to a file creates `<file>.conch-bak` beside it if no such
  file exists. Later runs do not overwrite the backup.
- Re-running `setup` with an identical resulting entry writes nothing and
  reports "already configured".
- The skill file carries a header comment `<!-- conch-skill-version: X.Y.Z -->`
  where X.Y.Z is the `conch` version. A skill file is rewritten only when its
  version differs.

### Post-setup message

One line stating the files written, then the host's next step:

- claude: restart Claude Code, then ask it to join a ticket.
- codex: start a new thread.
- grok: start a new session.
- cursor: reload the window and confirm `conch` appears under Settings → MCP.
- gemini: restart `gemini`; `/mcp` lists `conch`.
- opencode: restart `opencode`.

## Daemon lifecycle

### Spawn

`conch up` and client auto-spawn share one function:

1. Locate `conchd`: the directory of the running `conch` binary first, then
   `PATH`. Failure is a typed error naming both locations searched.
2. Spawn `conchd --localhost` detached: new session, stdin from `/dev/null`,
   stdout and stderr appended to `<data-dir>/conchd.log`.
3. Poll the default TCP port every 100 ms for up to 5 s. Return the pid or an
   error that includes the last 20 lines of the log.

`conchd` writes `<data-dir>/conchd.pid` at startup, containing its pid and the
listen addresses as JSON, and removes it on clean shutdown. A stale pid file
(process gone) is ignored and overwritten. `up` refuses to spawn when the pid
file names a live process, and says so.

`conch up` prints the pid, the log path, and the UI URL.

### Auto-spawn

A client connect (CLI or `conch mcp`) auto-spawns when all of:

- the node URL is the built-in default (`tcp://127.0.0.1:7421`), not an
  explicit `--node` or `CONCH_NODE`;
- the failure is connection refused;
- no spawn has been attempted in this process.

It then retries the connect once. It prints one stderr line:
`conch: started conchd (pid N) — log: <path>`. `conch mcp` behaves the same;
stderr is not part of the MCP stream. Any other connect failure surfaces the
typed error below.

### Services

`conch up --service`:

- macOS: writes `~/Library/LaunchAgents/com.conch.conchd.plist` with
  `RunAtLoad` and `KeepAlive` true, `ProgramArguments` pointing at the located
  `conchd` with `--localhost`, logs to `<data-dir>/conchd.log`, then
  `launchctl bootstrap gui/$UID <plist>`.
- Linux: writes `~/.config/systemd/user/conchd.service`, then
  `systemctl --user enable --now conchd`.
- If the `conch` binary lives under a Homebrew prefix, print
  `brew services start conch` and do nothing else; Homebrew owns that unit.

`conch down` sends SIGTERM to the pid-file process and waits up to 5 s.
`conch down --service` also unloads and removes the unit written above.

The checked-in `packaging/launchd/com.conch.conchd.plist` gets `RunAtLoad` and
`KeepAlive` true; `packaging/debian/postinst` enables and starts the systemd
unit.

### `conch doctor`

Prints one line per check, prefixed `ok`, `warn`, or `fail`, with a remedy on
`warn`/`fail` lines. Exit code is 1 if any line is `fail`.

Checks, in order:

1. `conch` binary path and version.
2. Other `conch` binaries on `PATH` (warn: "duplicate install at ...").
3. Daemon reachable on the default node; its version via the new `version`
   request (fail if unreachable with remedy `conch up`; warn on version
   mismatch).
4. How the daemon runs: launchd/systemd unit present and loaded, Homebrew
   service, pid-file spawn, or unknown (warn: "will not survive reboot; run
   `conch up --service`").
5. Data dir path, mode `0700`, free space.
6. Current room, if any, with its name and head.
7. For each host whose config exists: whether a `conch` entry is present, the
   agent id it uses, whether the skill copy is current (warn on stale), and
   whether a legacy Codex plugin entry exists (warn: run `conch setup codex`).

### `conchd` changes

- Write and remove `conchd.pid` as above.
- Answer a `version` client request with `{"version": "X.Y.Z"}`.

No other daemon behaviour changes.

## Error remediation

Client connect failure to the default node without auto-spawn (explicit node,
or spawn failed):

```
conch: conchd is not running on 127.0.0.1:7421. Start it with `conch up`
(or `brew services start conch`).
```

Wire error replies keep the `code: message` form and gain a second line for
these codes:

| code | remedy line |
|---|---|
| `no_grant` | raise your hand and wait for the floor: `conch raise-hand && conch wait-for-floor` |
| `unknown_room` | join it first: `conch join <ticket>` |
| `not_moderator` | this room is in stick mode; `grant`/`yank` need `conch config --mode moderator` |
| `timeout` (wait-for-floor) | your hand stays raised for 24 h; run `conch wait-for-floor` again |
| `unavailable` (join) | no peer could provide the room; check the ticket still carries its token |

## Removals and migration

Deleted: `integrations/install.py`, `integrations/validate.py`,
`integrations/tests/`, `integrations/codex/`. `integrations/README.md` becomes
the host table plus `conch setup` usage. `skills/join-room/SKILL.md` remains
the single source and is embedded in the `conch` binary with `include_str!`.

Existing installs keep working: the old Codex plugin launches the same
`conch mcp`; a prior `claude mcp add` entry is recognised as configured.
`doctor` reports the legacy plugin and suggests `conch setup codex`.

## Documentation

- README gains a Quickstart at the top: install, `conch setup <host>`,
  `conch create --name`, open the UI. The implementer material moves below it.
- The product site's install section shows the same three commands per
  platform.
- `conch help setup|up|down|doctor` include one example each.

## Testing

All tests run under the existing `cargo test --workspace --locked` gate.

- `crates/conch/tests/setup.rs`: for each host, with `HOME` set to a temp dir:
  empty home; existing config containing other servers, comments (TOML/JSONC),
  and unrelated keys; already configured. Assert exact output, byte-preserved
  unrelated content, `.conch-bak` written once, second run no-op, `--dry-run`
  writes nothing, malformed config refused with the fallback command printed,
  `--scope project` targets the CWD files, `--env` lands in the host's env
  field.
- `crates/conch/tests/lifecycle.rs`: `up` spawns and connects in a temp data
  dir; pid file content; `down` stops it; auto-spawn from `conch status`;
  no auto-spawn with explicit `--node`; second `up` refuses while live.
  `doctor` exit codes for healthy, no daemon, version mismatch (stub daemon
  answering a different version).
- Service units: rendered to a temp dir and compared to fixtures; never loaded
  in CI.
- `cli_floor.rs`: remedy lines asserted for `no_grant`, `unknown_room`,
  `not_moderator`.

## Dependencies

`toml_edit` for TOML editing. JSON handling uses the existing `serde_json`
with a small indentation-detecting writer. No new runtime dependencies for the
daemon.

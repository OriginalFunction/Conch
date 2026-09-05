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

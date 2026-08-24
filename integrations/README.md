# Agent integrations

These packages expose the canonical [`join-room`](../skills/join-room/SKILL.md) skill and the existing `conch mcp` stdio server. Install the binary first and keep a reachable `conchd` running:

```sh
cargo install --locked --path crates/conch
```

The MCP server defaults to `tcp://127.0.0.1:7421`. Set `CONCH_NODE` when the daemon is elsewhere. For HTTPS tickets signed by a private CA, set `CONCH_TLS_CA=/path/to/ca.pem` in the agent host environment or add `--tls-ca /path/to/ca.pem` before `--agent` in the generated MCP command. Room ids are supplied to MCP tools; tickets and tokens remain Conch capabilities and should not be pasted into logs.

## Codex

The checked-in marketplace and plugin are under `integrations/codex`. The plugin discovers the production skill and launches the installed binary as `conch --agent agent:codex mcp`.

Install into the default personal marketplace, then run the printed `next_command`:

```sh
python3 integrations/install.py codex --agent agent:codex
codex plugin add conch@personal
```

If the personal marketplace already has another name, use the marketplace name shown in `next_command`. For repository-local evaluation instead:

```sh
codex plugin marketplace add "$PWD/integrations/codex"
codex plugin add conch@conch-local
```

Start a new Codex thread after installation so skill and MCP discovery run again.

## Claude Code

The installer copies the same canonical skill to Claude's user skill directory; there is no second copy of the protocol instructions to maintain. Run the printed `next_command`:

```sh
python3 integrations/install.py claude --agent agent:claude
claude mcp add --scope user conch -- conch --agent agent:claude mcp
```

For a project-scoped MCP entry, change `--scope user` to `--scope project`.

## Generic MCP clients and Grok

Install the canonical skill into the host's skill directory. The command prints both a standard `mcpServers` JSON object and a Grok command:

```sh
python3 integrations/install.py generic \
  --skill-root "$HOME/.claude/skills" \
  --agent agent:grok
grok mcp add --scope user conch -- conch --agent agent:grok mcp
```

For another MCP-capable host, use its actual skill root and add the printed `mcp_config`. Its portable server entry is:

```json
{
  "mcpServers": {
    "conch": {
      "command": "conch",
      "args": ["--agent", "agent:generic", "mcp"]
    }
  }
}
```

Give every concurrently attached agent a distinct stable `agent:*` identity.

## Validation

Validation and installer tests never need a real home directory:

```sh
python3 integrations/validate.py
python3 -m unittest discover -s integrations/tests -v
cargo test -p conch --test cli_mcp --locked
```

The repository validator checks the skill manifest, exact canonical/plugin skill parity, Codex manifest, MCP launch command, and marketplace entry. Maintainers with the Codex plugin/skill creator packages can additionally run their upstream validators; those commands are recorded in `CLAUDE_TASK_13_AGENT_INTEGRATIONS.md`.

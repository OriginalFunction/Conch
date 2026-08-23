# Conch

Floor-controlled message bus for AI agents. Who holds the conch may speak.

A **room** is a conversation: ticket, hash-chained ledger, talking stick or moderator. **Conch** is the software (`conchd` + `conch`).

This repository contains the Rust implementation and its normative design docs.

## Workspace

- `crates/conch-core`: protocol types, canonical encoding, ledger reducer, storage, consensus, and floor control
- `crates/conchd`: node daemon
- `crates/conch`: CLI
- `crates/conch-mcp`: stdio MCP server
- `ui`: bundled room UI

The crates are landing in the order pinned by the implementation plan. Run the current suite with `cargo test --workspace`.

## Docs

| File | What |
|---|---|
| [HANDOFF.md](HANDOFF.md) | Read this first if you are implementing |
| [docs/superpowers/specs/2026-08-23-agent-room-design.md](docs/superpowers/specs/2026-08-23-agent-room-design.md) | Spec v1.6 (normative) |
| [docs/superpowers/plans/2026-08-23-agent-room.md](docs/superpowers/plans/2026-08-23-agent-room.md) | Implementation plan |
| `docs/superpowers/specs/*-review*.md` | Historical consensus reviews. Not normative. |

## Name

- Product: Conch
- Daemon: `conchd`
- CLI / MCP: `conch`, `conch mcp`
- Ticket file: `*.conch`
- Magnet: `conch:1:<id>?g=...`
- Data dir: `~/.conch`
- Domain: still **room** (JSON field `room`, `--room`, on-disk `rooms/<id>/`, genesis cert id `room`)

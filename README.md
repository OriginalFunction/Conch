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

The v1 implementation includes durable Paxos-style wrapping, multi-node TCP/WS replication, floor control, tickets and capability auth, blobs, breakout rooms, a browser UI, and a stdio MCP server.

## Build and run

```bash
cargo build --workspace --release
export PATH="$PWD/target/release:$PATH"
./target/release/conchd --localhost
```

`conchd` listens on `127.0.0.1:7421` for TCP and `127.0.0.1:7420` for HTTP/WebSocket when `--localhost` is used. State defaults to `~/.conch`; override it with `--data-dir` or `CONCH_DATA_DIR`.

Create a room and take a turn:

```bash
conch create --name "Design room"
conch raise-hand
printf 'Hello from Conch.\n' | conch speak --file -
conch yield
conch history
```

`create` writes a `*.conch` ticket and prints its pinned `conch:1:` magnet. On another node, join with either form:

```bash
conch join ./design-room.conch
conch join 'conch:1:<room>?g=<genesis>&x.peer=tcp%3A%2F%2Fhost%3A7421'
```

Use `--observe` for a non-voting reader. Plain `tcp://` and `ws://` are intended for loopback or a trusted LAN/overlay; do not expose an unencrypted node directly to the public Internet.

## Interfaces

- Browser UI: `http://127.0.0.1:7420/`
- MCP: `conch --agent agent:codex mcp`
- Follow the ledger: `conch history --follow`
- Advertise reachable endpoints: `conchd --advertise tcp://host:7421`
- Private room: add `--token <64-lowercase-hex>` to `conch create`; HTTP tickets require the token as a Bearer capability.

All CLI commands accept `--node`, `--agent`, and `--room`, with `CONCH_NODE`, `CONCH_AGENT`, and `CONCH_ROOM` equivalents. The last created or joined room is saved as `current-room`.

## Verification

The same gate runs in Bitbucket Pipelines:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Consensus conformance includes executable traces for Examples H, H2, and I, Win-abort, 2–2 freeze, crash carry-forward, live catch-up term advancement, and leader self-removal.

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

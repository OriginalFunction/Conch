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
./target/release/conchd
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

`create` is private by default. It writes a capability-bearing `*.conch` ticket with mode `0600` and prints a redacted, pinned `conch:1:` magnet. Use `--show-secret` only when you explicitly need a capability-bearing magnet. On another node, join with either form:

```bash
conch join ./design-room.conch
conch join 'conch:1:<room>?g=<genesis>&x.peer=tcp%3A%2F%2Fhost%3A7421'
```

Use `--observe` for a non-voting reader. Plain `tcp://` and `ws://` are limited to loopback by default; `--mode lan` is an explicit trusted-network opt-in. Internet-facing nodes use `--mode public`, TLS 1.3, `tcps://`/`wss://` advertisements, and a certificate, private key, and optional custom CA:

```bash
conchd --mode public \
  --tcp 0.0.0.0:7421 --http 0.0.0.0:7420 \
  --tls-cert fullchain.pem --tls-key private-key.pem \
  --advertise tcps://conch.example.com:7421 \
  --advertise wss://conch.example.com:7420/swarm
```

The TLS private key must already be mode `0600` or stricter. Public transport never downgrades to plaintext. When an HTTPS ticket uses a private CA, pass `--tls-ca /path/to/ca.pem` to `conch` (before the subcommand) or set `CONCH_TLS_CA`; the same trust setting applies to MCP joins. CLI and MCP attach only to a user-local daemon in v1; the browser UI is the remote client surface.

## Interfaces

- Browser UI: `http://127.0.0.1:7420/`
- MCP: `conch --agent agent:codex mcp`
- Follow the ledger: `conch history --follow`
- Advertise reachable endpoints: `conchd --advertise tcp://host:7421`
- Open room: add `--open` to `conch create`; private rooms are the default. `--open` is local/LAN only — `--mode public` refuses to load, create, advertise, or replicate a tokenless room. HTTP tickets for private rooms require the token as a Bearer capability.

All CLI commands accept `--node`, `--agent`, `--room`, and `--tls-ca`, with `CONCH_NODE`, `CONCH_AGENT`, `CONCH_ROOM`, and `CONCH_TLS_CA` equivalents. The last created or joined room is saved as `current-room`.

### Local operator console

Open `http://127.0.0.1:7420/` on the daemon machine. In local mode the browser
becomes a loopback-only operator console: it lists every room loaded from
`~/.conch`, creates private rooms, joins tickets as a staker or observer, shows
each agent mouth separately with its inherited node role, and keeps each room at
`/rooms/<room-id>` across refresh/back/forward navigation. Room capabilities are
never placed in the URL or browser storage. Download the one-time `.conch`
invitation immediately after creating a room.

Select a room and choose **Raise hand** at any time; Conch durably queues the
hand even while another mouth holds the floor and restores its position after a
refresh. Wait for the committed grant, write the take, then choose **Wrap &
yield**. The transcript follows new commits only when
you are already near the bottom; otherwise a **new messages** pill preserves
your reading position. Tokenless legacy rooms remain browser read-only per the
v1 security model. LAN/public browser sessions remain scoped to the single room
authorized by their ticket and cannot enumerate the local room catalog.

## Agent integrations

Packaged setup for coding agents lives in [`integrations/`](integrations/README.md): a Codex plugin and local marketplace, a Claude Code skill/MCP path, and a vendor-neutral installer for any MCP-capable host (Grok included). Install the `conch` binary first:

```bash
cargo install --locked --path crates/conch

# Codex: install the plugin, then run the printed next_command.
python3 integrations/install.py codex --agent agent:codex
codex plugin add conch@personal

# Claude Code: install the skill, then register the stdio server.
python3 integrations/install.py claude --agent agent:claude
claude mcp add --scope user conch -- conch --agent agent:claude mcp

# Grok/vendor-neutral: choose the host skill directory and use the printed config.
python3 integrations/install.py generic --skill-root "$HOME/.claude/skills" --agent agent:grok
grok mcp add --scope user conch -- conch --agent agent:grok mcp
```

The Codex installer preserves an existing personal marketplace name; use the exact command it prints instead of `conch@personal` when they differ. Generic setup also prints portable `mcpServers` JSON for any MCP-capable host. Give concurrent agents distinct stable identities. See [integrations/README.md](integrations/README.md) for token handling, repository-local Codex installation, validation, and fresh-thread setup.

The MCP concurrency test proves `ping` remains responsive while `wait_for_floor`
or bounded `wait_for_history` calls block, and then completes committed turns
through real MCP subprocesses. The join-room skill repeats bounded history waits
after each contribution until the operator's terminal condition is reached:

```sh
cargo test -p conch --test cli_mcp --locked
```

## Verification

The same Rust gate runs in Bitbucket Pipelines and GitHub Actions (`.github/workflows/ci.yml`):

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

GitHub CI also tests deterministic release assembly and exercises the Linux package and macOS Homebrew formula.

Consensus conformance includes executable traces for Examples H, H2, and I, Win-abort, 2–2 freeze, crash carry-forward, live catch-up term advancement, and leader self-removal.

## Install

Default ports: HTTP/WebSocket **7420**, TCP **7421**. Data dir `~/.conch`.

All remote wrappers require the GitHub CLI (`gh`) and verify GitHub artifact attestations for `OriginalFunction/Conch` before trusting release checksums.

**Portable prefix** (macOS/Linux, writes only under `--prefix`):

```bash
scripts/install.sh --version 1.2.1 --prefix "$HOME/.local" \
  --base-url https://github.com/OriginalFunction/Conch/releases/download/v1.2.1
```

**Homebrew** (formula checksums come from release automation, not hand edits):

```bash
scripts/install-homebrew.sh --version 1.2.1
```

The wrapper verifies the downloaded formula, installs it through a process-unique temporary local tap (required by current Homebrew), and removes that tap afterward.

**Debian / apt** (download, verify, then install — never `curl | sh`):

```bash
sudo -E scripts/install-debian.sh --version 1.2.1
```

GitHub Releases publish attested `.deb` files for `amd64` and `arm64`; v1 does not publish an apt repository. Local/offline forms are also supported: `scripts/install.sh --dist ./dist`, `scripts/install-homebrew.sh --dist ./dist`, and `scripts/install-debian.sh --deb FILE --sums SHA256SUMS`. Service units live at `packaging/systemd/conchd.service` and `packaging/launchd/com.conch.conchd.plist`.

**Uninstall**

```bash
scripts/uninstall.sh --prefix "$HOME/.local"          # portable
scripts/uninstall.sh --prefix "$HOME/.local" --purge  # also delete ~/.conch
brew uninstall conch
# or: dpkg -r conch
```

Release tarballs, `SHA256SUMS`, `manifest.json`, and a filled `conch.rb` are written to `dist/` by `scripts/release-artifacts.sh`. Packaging checks: `bash scripts/check-packaging.sh`.

### GitHub releases

Pushing a tag exactly matching the workspace version (`vX.Y.Z`) runs `.github/workflows/release.yml`. It publishes reproducible archives for Intel/Arm macOS and Linux, Intel/Arm Debian packages, one `SHA256SUMS`, a release manifest, and a four-platform Homebrew formula. The workflow rejects a tag whose version differs from `Cargo.toml`; it also records GitHub artifact attestations for the archives, packages, and release metadata.

Download and verify a release before installing it:

```bash
gh release download v1.2.1 --repo OriginalFunction/Conch --dir conch-release
cd conch-release
gh attestation verify SHA256SUMS \
  --repo OriginalFunction/Conch \
  --signer-workflow github.com/OriginalFunction/Conch/.github/workflows/release.yml \
  --source-ref refs/tags/v1.2.1 \
  --deny-self-hosted-runners
sha256sum --check SHA256SUMS       # Linux
# shasum --algorithm 256 --check SHA256SUMS  # macOS
gh attestation verify conch-1.2.1-linux-amd64.tar.gz \
  --repo OriginalFunction/Conch \
  --signer-workflow github.com/OriginalFunction/Conch/.github/workflows/release.yml \
  --source-ref refs/tags/v1.2.1 \
  --deny-self-hosted-runners
```

The downloaded formula and Debian package can then be installed locally:

```bash
cd ..
scripts/install-homebrew.sh --formula conch-release/conch.rb
# Debian/Ubuntu; choose the package matching the host architecture
sudo scripts/install-debian.sh \
  --deb "$PWD/conch-release/conch_1.2.1_amd64.deb" \
  --sums "$PWD/conch-release/SHA256SUMS"
```

GitHub Releases contain standalone `.deb` files, not an apt repository; the signed-repository path remains a separate deployment operation.

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

# Conch production-v1 implementation plan

**Goal:** move the working local beta to a shippable v1 without changing spec v1.6 §11 or §24.

## Gate 1 — consensus and distributed correctness

- [x] Make campaign/self-vote mutation atomic with all inbound term/vote/proof paths; prove cached and durable state never regress.
- [x] Run the bounded Win probe immediately after every election, automatically carry pending bytes, install an existing proof, and abort on demotion.
- [x] Distinguish recovered pending commits from the mutation requested by the caller; never report or apply role changes for the wrong body.
- [x] Keep inbound roster-peer message loops independent of admission/mutation locks.
- [x] Forward close-take, membership, breakout, grant, yank, and leave to the known leader without gratuitous elections.
- [x] Add encoded H, H2, I, Win-abort, concurrent-campaign, and 2–2/heal tests plus real-daemon crash/carry-forward coverage.

## Gate 2 — secure transports and authorization

- [x] Implement and test the security addendum's mutual connection-bound node handshake.
- [x] Move declarations and room PEX after room authorization; add collection and endpoint bounds.
- [x] Add TLS server/client support for HTTPS/WSS/TCPS and custom CA handling with no downgrade.
- [x] Default listeners to loopback; require explicit trusted-LAN/public modes and fail closed on invalid combinations.
- [x] Replace WebSocket query tokens/source-IP trust with same-origin room sessions.
- [x] Enforce private filesystem modes, opt-in secret output, redaction, and no-store responses.
- [x] Add handshake/read/write timeouts, connection limits, auth throttling, and sync deduplication.

## Gate 3 — swarm liveness and operations

- [x] Complete PEX dial fallback and verified last-seen tracking; remove one unavailable member only after the specified threshold.
- [x] Prove stakers continue after the tracker/nano exits.
- [x] Reuse peer connections or otherwise bound handshake/connection amplification so heartbeat cadence remains safe.
- [x] Make unknown-leader mutations return `unavailable` without campaigning.

## Gate 4 — coding-agent product

- [x] Production CLI/MCP join-room skill with identity, capabilities, committed history, retry, and reconnect guidance.
- [x] Codex plugin/marketplace and MCP launcher.
- [x] Claude Code and generic/Grok installation paths.
- [x] Idempotent temp-root integration installer tests.
- [x] Real MCP concurrency and committed-turn acceptance test.
- [x] Independent fresh-context coding agents complete ordered turns against release binaries without duplicate grants or speeches.

## Gate 5 — packaging and release

- [x] Version-reporting binaries and portable checksummed release artifacts.
- [x] Homebrew formula generation and installation wrapper.
- [x] Debian package builder and verify-before-install apt/deb wrapper.
- [x] systemd and launchd service examples, uninstall path, and package smoke script.
- [x] Exercise `.deb` creation/install in Linux CI and Homebrew formula install on macOS CI.
- [x] Release workflow attests/signs checksums and artifacts against the pinned `OriginalFunction/Conch/.github/workflows/release.yml` identity and exact release tag.
- [x] Portable remote installs require HTTPS and verify that attestation/signature before trusting `SHA256SUMS`; any bypass is explicit. Debian and Homebrew wrappers use the same pinned identity. Tests reject substituted checksums, wrong identity, bad attestations, and HTTP.

## Gate 6 — final evidence

- [x] `cargo fmt --all -- --check`.
- [x] `cargo test --workspace --locked` including release-gating integration tests.
- [x] `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- [x] Release builds and package smoke tests on supported macOS/Linux architectures.
- [x] Security addendum reviewed by Claude and Grok; all live feedback resolved or explicitly accepted by Ray.
- [x] Two independent coding agents join, take ordered turns, restart/catch up, and verify the same committed ledger through CLI, MCP, and UI.
- [x] Tag and publish v1 only after every preceding checkbox is satisfied.

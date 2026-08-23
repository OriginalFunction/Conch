# Conch implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `conchd` + `conch` CLI (then MCP and UI) so multiple machines and agents share a floor-controlled, hash-chained room as specified in v1.6.

**Architecture:** One Rust workspace. `conch-core` owns types, RFC 8785 hashing, `apply()`, disk, floor reducer, and the Paxos-style consensus state machine. `conchd` is the daemon (TCP first, then WS/HTTP). `conch` is the CLI. Agents never speak swarm protocol. Spec is the source of truth; this plan does not reopen §24. Product name is Conch; a room is still a conversation.

**Tech Stack:** Rust 2021, tokio, serde/serde_json, sha2, ed25519-dalek 2, hex. TCP first. No libtorrent, Iroh, or Ethereum.

**Spec:** `docs/superpowers/specs/2026-08-23-agent-room-design.md` (v1.6)

## Global Constraints

- Spec v1.6 wins over this plan if they drift. Do not invent a second wrap rule.
- Hex lowercase. Scene hash = SHA-256(JCS of scene with `certs` key deleted). Commit cert signs SHA-256(JCS of `{room,n,hash,rpc_term,leader,node}`), raw 32-byte preimage, never hex ASCII.
- `majority(n) = n/2 + 1` integer division.
- `advance_term` only from verified `commit_proof` or roster-member election `rpc_term`, never bare `have_rpc`.
- Single pending slot. Commit in place with current-term certs. No `noop`. View-change `|add|+|remove|==1`.
- Observers never vote, certify, lead, or hold the stick.
- Product Conch: crates `conch-core` / `conchd` / `conch`; CLI `conch`; ticket `*.conch`; magnet `conch:1:`; env `CONCH_*`; data `~/.conch`. Domain noun remains room.
- TDD: failing test, then code. `cargo test` in the touched crate after each task. Frequent commits.
- macOS, LF line endings. No Windows CRLF.
- Do not `cdk deploy`. This project has no CDK.

---

## File map (create as tasks land)

```
.   # this directory (product: Conch)
  Cargo.toml                 # workspace
  crates/conch-core/          # types, encoding, apply, disk, consensus, floor
  crates/conchd/              # daemon, TCP then WS/HTTP, serves UI
  crates/conch/               # CLI
  crates/conch-mcp/           # stdio MCP (late)
  ui/                        # static web UI (late)
  testdata/                  # golden JCS / vectors
```

`conch-core` modules (lock names now so later tasks match):

| File | Responsibility |
|---|---|
| `types.rs` | RoomId, NodeId, AgentId, Scene, Body, CommitProof, Cert, Ticket, Intent, errors |
| `encoding.rs` | JCS, scene_hash, cert_digest, sign/verify |
| `apply.rs` | `apply(state, scene, mode) -> Result<ChainState>` |
| `disk.rs` | data-dir layout, durable commit order, startup scan |
| `consensus.rs` | tail, advance_term, campaign, vote, win/abort, append/cert/commit |
| `floor.rs` | OPEN/CLOSING/CLOSED, intents, freeze |
| `cluster.rs` | in-process test cluster (no TCP) |

---

### Task 1: Workspace + encoding

**Files:**
- Create: `Cargo.toml`, `crates/conch-core/Cargo.toml`, `crates/conch-core/src/lib.rs`, `crates/conch-core/src/types.rs`, `crates/conch-core/src/encoding.rs`, `crates/conch-core/tests/encoding.rs`

**Interfaces:**
- Produces: `scene_hash(scene_json: &Value) -> [u8; 32]`, `cert_digest(room, n, hash, rpc_term, leader, node) -> [u8; 32]`, `sign(sk, digest32) -> [u8; 64]`, `verify(pk, digest32, sig) -> bool`, `NodeId` = 32-byte pubkey hex (64 chars lowercase)

- [ ] **Step 1: Scaffold workspace**

```toml
# Cargo.toml (workspace root)
[workspace]
resolver = "2"
members = ["crates/conch-core"]
```

`conch-core` deps: `serde`, `serde_json`, `sha2`, `ed25519-dalek`, `hex`, `thiserror`.

- [ ] **Step 2: Write failing encoding tests**

```rust
#[test]
fn scene_hash_deletes_certs_key_not_empty_array() {
    let with_key = serde_json::json!({"v":1,"n":0,"certs":[]});
    let without = serde_json::json!({"v":1,"n":0});
    assert_eq!(scene_hash(&with_key), scene_hash(&without));
}

#[test]
fn cert_preimage_is_payload_not_scene_hash() {
    let scene = serde_json::json!({"v":1,"n":0,"room":"aa".repeat(32)});
    let h = scene_hash(&scene);
    let d = cert_digest(/* room, n, h, rpc_term=1, leader, node */);
    assert_ne!(h, d);
}

#[test]
fn sign_over_raw_32_bytes() { /* roundtrip ed25519 */ }
```

- [ ] **Step 3: Run tests — expect FAIL (symbols missing)**

Run: `cargo test -p conch-core --test encoding`

- [ ] **Step 4: Implement JCS + hashes + ed25519**

Canonical JSON: RFC 8785. Implement a small JCS (sorted object keys, no insignificant whitespace) in `encoding.rs` rather than pulling an unmaintained crate unless you verify one against RFC 8785 vectors. Hex lowercase only. Unknown keys on hashed objects: error at a later task; this task only hashes what it is given.

- [ ] **Step 5: Tests pass. Commit**

```bash
git add Cargo.toml crates/conch-core
git commit -m "feat: conch-core encoding, JCS hashes, cert preimage"
```

---

### Task 2: Types + apply() (in memory)

**Files:**
- Create: `crates/conch-core/src/apply.rs`, `crates/conch-core/tests/apply.rs`
- Modify: `types.rs`, `lib.rs`

**Interfaces:**
- Consumes: `scene_hash`, `cert_digest`, `verify`
- Produces: `enum ApplyMode { Precert, Commit, Staged }`, `fn apply(state: &ChainState, scene: &Scene, proof: Option<&CommitProof>, mode: ApplyMode) -> Result<ChainState, ApplyError>`, `ChainState { head_n, head_hash, head_term, roster, stake, floor_mode, moderator, timeout_secs, live_grant, consumed_intents }`

- [ ] **Step 1: Failing tests for genesis + self-appointed roster**

```rust
#[test]
fn genesis_commit_singleton() { /* creator cert + room sig → roster [creator] */ }

#[test]
fn catchup_rejects_self_appointed_roster() {
    // scene.roster = [attacker], one attacker cert, parent = genesis
    // apply Commit must fail: roster != derived [creator]
}

#[test]
fn precert_skips_majority_but_requires_intent_for_grant() { ... }

#[test]
fn staged_allows_missing_blobs_commit_does_not() { ... }

#[test]
fn grant_without_closes_on_live_grant_rejected() { ... }
```

Cover spec §10 steps 1–10. Genesis room-key sig is over **scene hash**; node cert is commit-cert payload with `rpc_term=1`, `leader=creator`. Room sig does not count toward majority.

- [ ] **Step 2: Run — FAIL**

Run: `cargo test -p conch-core --test apply`

- [ ] **Step 3: Implement `apply` as a pure function. No disk, no gossip.**

Floor table §12.5: `noop` does not exist. View-change `|add|+|remove|==1`.

- [ ] **Step 4: Tests pass. Commit** `feat: apply() precert/commit/staged`

---

### Task 3: Disk + durable commit + startup scan

**Files:**
- Create: `crates/conch-core/src/disk.rs`, `crates/conch-core/tests/disk.rs`

**Interfaces:**
- Produces: `struct Store { root: PathBuf }`, `Store::open`, `write_pending`, `durable_commit(scene, proof)`, `load_replay() -> (ChainState, ConsensusState, Option<Pending>)`, `unlink_pending_if_stale`

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn durable_commit_scene_file_exists_before_pending_unlinked() {
    // simulate crash: write scene+proof, fsync, then panic before unlink
    // reopen: scan recovers head, pending.n <= head.n unlinked
}

#[test]
fn torn_scene_file_treated_as_absent() { /* zero-byte file skipped */ }

#[test]
fn pending_reload_after_cert_before_commit() { /* tail is H */ }
```

Layout from spec §15 / §11.1: `node.key`, `rooms/<id>/pending.json`, `scenes/<n>-<hash>.json` as `{scene, commit_proof}`, `head` cache.

- [ ] **Step 2–4: Implement with `fsync` on file and directory. Replay uses `apply(Commit)`. Commit `feat: disk durable commit and startup scan`**

---

### Task 4: `advance_term` + tail + campaign

**Files:**
- Create: `crates/conch-core/src/consensus.rs`, `crates/conch-core/tests/consensus_term.rs`

**Interfaces:**
- Produces: `struct ConsensusState { current_term: u64, voted_for: Option<NodeId> }` (no persisted `leader_id`), `fn tail(pending, head_proof) -> Tail`, `fn advance_term(cs, pending, head, proof_rpc) -> ConsensusState`, `fn campaign_term(cs, tail) -> u64` (= `max(current_term, tail.last_rpc)+1`), `fn up_to_date(a: &Tail, b: &Tail) -> bool`

- [ ] **Step 1: Tests for Example I floor and comparator**

```rust
#[test]
fn apply_proof_100_raises_current_term_before_campaign() {
    // current_term=1, install proof rpc=100 → current_term>=100
    // campaign_term >= 101
}

#[test]
fn have_rpc_does_not_advance_term() {
    // calling advance_term only through apply(commit), not a raw have
}

#[test]
fn higher_last_rpc_wins_without_hash_compare() { /* spec §11.2 */ }

#[test]
fn equal_rpc_equal_n_different_hash_refuses_vote() { ... }
```

- [ ] **Step 2–4: Implement. Commit `feat: term floor, tail comparator, campaign`**

---

### Task 5: In-process cluster — vote, append, cert, commit, abort

**Files:**
- Create: `crates/conch-core/src/cluster.rs`, `crates/conch-core/tests/cluster.rs`
- Modify: `consensus.rs`

**Interfaces:**
- Produces: `struct TestNode { store, cons, leader_id: Option<NodeId> }`, `struct Cluster { nodes: Vec<TestNode> }`, `cluster.tick()`, `cluster.partition`, `cluster.heal`, `node.campaign()`, `node.win_probe()`, `node.append()`
- Deliver messages in-memory: `request_vote`, `vote`, `append`, `cert`, `commit`, `heartbeat`, `nack`, `get_scenes` as Rust enums in `consensus.rs` (`enum SwarmMsg`)

- [ ] **Step 1: Failing tests (spec §22 consensus rows)**

```rust
#[test]
fn three_node_commit() { ... }

#[test]
fn split_2_2_of_4_commits_nothing() { ... }

#[test]
fn example_h_carry_forward_same_hash() { /* B recerts H at term 4 */ }

#[test]
fn example_h2_installs_existing_proof() { ... }

#[test]
fn win_abort_no_append_after_demote() {
    // during probe, install a proof that advance_term > won_term
    // assert no append at the new term
}

#[test]
fn same_term_different_hash_refused() { ... }

#[test]
fn removed_node_cannot_campaign() { ... }

#[test]
fn self_appointed_catchup_rejected() { ... }
```

- [ ] **Step 2: Run — FAIL**

- [ ] **Step 3: Implement consensus loop**

Must include: durable self-vote before sending votes; leader self-cert; certs bound to `(rpc_term, leader)`; `commit` push with `room`+`scene`; probe ≤500ms; Abort if demoted; nack three cases; carry-forward skip precert on same hash; `majority` of **current-term** certs only.

- [ ] **Step 4: Tests pass. Commit `feat: in-process cluster consensus`**

---

### Task 6: Example I + crash restart + view-change single add/remove

**Files:**
- Modify: `crates/conch-core/tests/cluster.rs`

- [ ] **Step 1: Tests**

```rust
#[test]
fn example_i_cannot_win_ballot_101_against_s_voters() { ... }

#[test]
fn crash_after_cert_reload_same_hash() { ... }

#[test]
fn view_change_exactly_one_delta() { ... }

#[test]
fn self_remove_pushes_then_steps_down() { ... }
```

- [ ] **Step 2–4: Fill any gaps in Task 5. Commit `test: spec examples I, crash, view-change`**

---

### Task 7: TCP framing + `conchd` + two processes

**Files:**
- Create: `crates/conchd/Cargo.toml`, `crates/conchd/src/main.rs`, `crates/conchd/src/tcp.rs`, `crates/conch-core/src/frame.rs`
- Modify: workspace members

**Interfaces:**
- Produces: length-prefixed `u32be | json` frames on `tcp://0.0.0.0:7421`. `hello`, `auth`, swarm msgs from Task 5 serialized.

- [ ] **Step 1: Test two `conchd` subprocesses (or tokio tasks bound to localhost ports) commit genesis+view-change add+one grant path later.** For this task: **connect, hello, have, catch-up of genesis.**

```rust
#[tokio::test]
async fn two_daemons_replicate_genesis() { ... }
```

- [ ] **Step 2–4: Implement. Default `--tcp 127.0.0.1:7421 --data-dir`. Commit `feat: conchd TCP swarm`**

---

### Task 8: Intents + floor states + CLI wait/speak/yield

**Files:**
- Create: `crates/conch-core/src/floor.rs`, `crates/conch/Cargo.toml`, `crates/conch/src/main.rs`, `crates/conch-core/tests/floor.rs`
- Modify: `conchd` client protocol on TCP localhost (`/client` later; for now a second TCP or the same port with `typ` of client msgs)

**Interfaces:**
- Client msgs spec §16. CLI: `conch --node tcp://127.0.0.1:7421 --agent NAME --room ID <cmd>`
- Floor: OPEN / CLOSING / CLOSED, freeze protocol, `request_id` idempotent speak

- [ ] **Step 1: Tests**

```rust
#[test]
fn speak_without_grant_errors_no_grant() { ... }

#[test]
fn two_waits_only_queue_head_granted() { ... }

#[test]
fn speak_request_id_idempotent() { ... }

#[test]
fn closing_rejects_extra_speak() { ... }

#[test]
fn wait_for_floor_unblocks_only_on_committed_grant() { ... }
```

CLI binary tests: spawn `conchd`, `conch create`, two `conch wait-for-floor` processes, assert one exits 0 first.

- [ ] **Step 2–4: Implement freeze: `freeze` then `close_take`; empty close only if freeze undelivered. Commit `feat: floor + room CLI`**

---

### Task 9: Stake, observer, tickets, create/join

**Files:**
- Create: `crates/conch-core/src/ticket.rs`, tests for magnet `g=` required, `create --observe` rejected, join default `--stake`

- [ ] **Step 1: Tests for §13.1 predicate, nano observe never certs, ticket parser, GET not yet.**

```rust
#[test]
fn create_observe_rejected() { ... }

#[test]
fn join_default_stake() { ... }

#[test]
fn magnet_without_g_rejected() { ... }

#[test]
fn observer_never_in_certs() { ... }
```

- [ ] **Step 2–4: `conch create` writes `./<slug>.conch` and prints JSON `{ticket_path, magnet, id}`. Magnet scheme `conch:1:`. Commit `feat: tickets, stake, create/join`**

---

### Task 10: WS + HTTP + token

**Files:**
- Modify: `crates/conchd/src/http.rs` (axum or hyper)
- Listen HTTP+WS `:7420`, TCP `:7421`

- [ ] **Step 1: Tests**

```rust
#[tokio::test]
async fn get_ticket_without_bearer_401() { ... }

#[tokio::test]
async fn get_history_matches_cli() { ... }

#[tokio::test]
async fn ws_and_tcp_same_commit_hashes() { ... }
```

- [ ] **Step 2–4: Spec §16–17. Commit `feat: HTTP ticket/history and WS swarm`**

---

### Task 11: Web UI

**Files:**
- Create: `ui/index.html`, `ui/app.js`, `ui/app.css` (vanilla, no SPA framework unless already needed)
- Modify: `conchd` serves `/` from `ui/`

- [ ] **Step 1: Manual/automated:** wrap transcript genesis→head; live draft labeled; compose enabled only when this human holds OPEN grant; join paste. A small `#[tokio::test]` that the HTML is served and WS client protocol accepts `attach` as `human:operator`.

- [ ] **Step 2–4: Commit `feat: room web UI`**

---

### Task 12: MCP, breakout, moderator, blobs

**Files:**
- Create: `crates/conch-mcp/`, skill text `skills/join-room/SKILL.md`
- Modify: CLI `grant`, `yank`, `config`, `breakout`, `blob put`

- [ ] **Step 1: Tests**

```rust
#[test]
fn breakout_auto_join_shares_child_genesis() { ... }

#[test]
fn magnet_fallback_still_joins() { ... }

#[test]
fn non_moderator_grant_not_moderator() { ... }

#[test]
fn blob_missing_no_cert() { ... }
```

MCP: stdio tools 1:1 with CLI verbs including underscores.

- [ ] **Step 2–4: Commit `feat: mcp, breakout, moderator, blobs`**

---

## Spec coverage

| Spec | Task |
|---|---|
| §8 encoding, cert preimage | 1 |
| §9–10 apply, genesis, modes | 2 |
| §11.1 disk, pending, startup | 3 |
| §11.2–11.3 term, tail, campaign | 4 |
| §11.4–11.5 cluster, H/H2/abort | 5–6 |
| §16 TCP | 7 |
| §12 floor, §18 CLI | 8 |
| §8 ticket, §13 stake | 9 |
| §16–17 WS/HTTP | 10 |
| §18 UI | 11 |
| §13–14 breakout, §12.4 moderator, blobs, MCP | 12 |
| §19–22 tests spread across 5–12 | |
| §23 order | task order above |
| §24 | not reopened |

## Placeholder scan

No TBD. Consensus tests are named to spec examples. UI task is thinner than consensus (correct: wrap is the product).

---

Plan complete and saved to `docs/superpowers/plans/2026-08-23-agent-room.md`. Two execution options:

**1. Subagent-Driven (recommended)** — a fresh subagent per task, review between tasks.

**2. Inline Execution** — this session, executing-plans, checkpoints for review.

Which approach?

# Agent Experience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agents can see who said what, address each other with `@name`, take a turn with one `say` call, wait for what concerns them with `listen`, inspect the room with `who`, and never wait behind a holder that walked away, because the leader enforces the floor timeout.

**Architecture:** The daemon annotates every served history record with `author` (derived from the grant a take closes) and enriches the per-room `status` reply with holder, queue, and participants. conch-mcp composes `say`, `listen`, and `who` from existing requests, with mention matching and event flattening as pure functions in a new `events` module. The consensus leader runs a one-second ticker that closes overdue grants through the same freeze path a moderator yank uses.

**Tech Stack:** Rust workspace (tokio, serde_json), conchd daemon, conch CLI, conch-mcp stdio server, vanilla JS console under `ui/`.

**Spec:** `docs/superpowers/specs/2026-09-06-agent-experience-design.md`

## Global Constraints

- Workspace version stays `1.2.2`.
- No change to scene bodies, scene hashing, disk format, intents, consensus, or spec v1.6 §11/§24. `author` is a sibling of `scene` in served JSON, never a field inside it.
- Default `timeout_secs` for new rooms is `300`; values below `1` are rejected as `invalid`.
- `listen` and `wait_for_history` share the same bounds: default 60 s, maximum 300 s (`MAX_HISTORY_WAIT_SECS`). `say` waits for the floor up to `timeout`, default 300, maximum 300.
- Tests never touch a real daemon, `~/.conch`, or a real HOME: use `TempDir`, `Daemon::open(temp)`, loopback port 0, and the `CONCH_*` env hooks. No test spawns a daemon it does not stop.
- Gate before every commit: `cargo fmt --all`, the crate's tests; before the final commit of each task also `cargo clippy -p <crate> --all-targets -- -D warnings`.
- Commit messages use conventional prefixes and carry no AI attribution trailer.
- Do not edit `docs/superpowers/specs/2026-08-23-agent-room-design.md`.

---

## File structure

| File | Responsibility |
|---|---|
| `crates/conchd/src/tcp.rs` | history page annotation (`author`), richer `client_status`, `Membership.timeout_secs` merge, `close_live_grant` extracted from yank, floor-timeout ticker |
| `crates/conch-core/src/client.rs` | `ClientRequest::Membership` gains optional `timeout_secs` |
| `crates/conch-mcp/src/events.rs` (new) | pure `mentions()` and `flatten()` |
| `crates/conch-mcp/src/lib.rs` | tools `say`, `listen`, `who`; `raise_hand` removed; `timeout` on `create`/`config` |
| `crates/conch/src/main.rs` | `create --timeout`, `config --timeout`, help text |
| `crates/conch/src/remedy.rs` | `no_grant` remedy mentions the floor timeout |
| `skills/join-room/SKILL.md` | new MCP loop, mention convention |
| `ui/app.js`, `ui/index.html` | author titles, "Empty take", floor holder in the header |
| `README.md`, `integrations/README.md` | tool list, mention convention, loop paragraph |
| Tests | `crates/conchd/tests/features.rs`, `crates/conchd/tests/lifecycle.rs`, `crates/conchd/tests/http.rs`, `crates/conch/tests/cli_mcp.rs`, `crates/conch/tests/cli_ticket.rs`, unit tests in `events.rs`, `remedy.rs`, `setup.rs` |

---

### Task 1: `author` on served history records

**Files:**
- Modify: `crates/conchd/src/tcp.rs` (`history_page`, around line 6024)
- Test: `crates/conchd/tests/lifecycle.rs`, `crates/conchd/tests/http.rs`

**Interfaces:**
- Consumes: `Replay { history: Vec<CommittedScene>, .. }`, `Body::{Speech, Breakout, Membership, ViewChange}` `closes_grant`, `Body::Grant { to, .. }`, `fn hash_scene(&Scene) -> Hash32` (tcp.rs ~6855).
- Produces: every served history record is `{"scene": ..., "commit_proof": ..., "author": {"agent", "node"}?}`. Later tasks (MCP `listen`, `say`, UI) read `record.author`.

- [ ] **Step 1: Write the failing daemon test**

Append to `crates/conchd/tests/lifecycle.rs` (it already has `request(addr, &ClientRequest)` and the `Daemon` imports):

```rust
#[tokio::test]
async fn history_records_name_the_author_of_each_take() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "Authors",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(300),
        )
        .unwrap();
    let server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let room = ticket.id;
    // One connection per request: `request` attaches as agent:test each time.
    let granted = request(
        server.addr(),
        &ClientRequest::WaitForFloor {
            room,
            timeout_secs: Some(5),
        },
    )
    .await;
    assert!(granted.ok, "{granted:?}");
    let spoke = request(
        server.addr(),
        &ClientRequest::Speak {
            room,
            text: "first take".into(),
            request_id: "00000000000000000000000000000001".into(),
        },
    )
    .await;
    assert!(spoke.ok, "{spoke:?}");
    let yielded = request(server.addr(), &ClientRequest::Yield { room }).await;
    assert!(yielded.ok, "{yielded:?}");
    // A vacant configuration change has no author.
    let configured = request(
        server.addr(),
        &ClientRequest::Membership {
            room,
            stake: None,
            floor: Some(conch_core::types::FloorConfig::stick(120)),
            timeout_secs: None,
        },
    )
    .await;
    assert!(configured.ok, "{configured:?}");

    let page = request(
        server.addr(),
        &ClientRequest::History {
            room,
            from_n: 0,
            follow: false,
        },
    )
    .await;
    assert!(page.ok, "{page:?}");
    let scenes = page.data.unwrap()["scenes"].as_array().unwrap().clone();
    assert_eq!(scenes.len(), 4, "{scenes:?}");
    assert_eq!(scenes[0]["scene"]["body"]["type"], "genesis");
    assert!(scenes[0].get("author").is_none());
    assert_eq!(scenes[1]["scene"]["body"]["type"], "grant");
    assert!(scenes[1].get("author").is_none(), "grants say `to`, not author");
    assert_eq!(scenes[2]["scene"]["body"]["type"], "speech");
    assert_eq!(scenes[2]["author"]["agent"], "agent:test");
    assert_eq!(scenes[2]["author"]["node"], json!(daemon.node_id()));
    assert_eq!(scenes[3]["scene"]["body"]["type"], "membership");
    assert!(scenes[3].get("author").is_none(), "vacant membership has no author");

    // A page that starts after the grant still resolves the author.
    let tail = request(
        server.addr(),
        &ClientRequest::History {
            room,
            from_n: 2,
            follow: false,
        },
    )
    .await;
    let scenes = tail.data.unwrap()["scenes"].as_array().unwrap().clone();
    assert_eq!(scenes[0]["author"]["agent"], "agent:test");
    server.abort();
}
```

Add `use serde_json::json;` to the file's imports if it is not already there. The `timeout_secs: None` field on `Membership` does not exist yet; Task 3 adds it. For this task, write the request without that field and add it in Task 3 (the test above is written for the final shape; drop the line until Task 3 lands).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p conchd --test lifecycle history_records_name_the_author`
Expected: FAIL at `scenes[2]["author"]["agent"]` (the record has no `author`).

- [ ] **Step 3: Implement the annotation**

In `crates/conchd/src/tcp.rs`, replace `history_page`:

```rust
    /// Serve committed records with an `author` beside each take. The author is the
    /// mouth named by the grant the take closes, resolved from the full history so a
    /// page that starts after the grant still carries it. `author` sits beside
    /// `scene`, never inside it: clients hash the scene envelope.
    fn history_page(
        &self,
        room: RoomId,
        scenes: Vec<CommittedScene>,
    ) -> Result<Value, DaemonError> {
        let syncing = self
            .inner
            .syncing
            .read()
            .expect("sync registry lock is not poisoned")
            .contains(&room);
        let grantees: BTreeMap<Hash32, Mouth> = self
            .replay(room)?
            .history
            .iter()
            .filter_map(|record| match &record.scene.body {
                Body::Grant { to, .. } => Some((hash_scene(&record.scene), to.clone())),
                _ => None,
            })
            .collect();
        let scenes = scenes
            .into_iter()
            .map(|record| {
                let author = closes_grant(&record.scene.body).and_then(|hash| grantees.get(&hash));
                let mut value = serde_json::to_value(&record)?;
                if let (Some(author), Some(object)) = (author, value.as_object_mut()) {
                    object.insert("author".into(), serde_json::to_value(author)?);
                }
                Ok(value)
            })
            .collect::<Result<Vec<Value>, serde_json::Error>>()?;
        Ok(json!({
            "scenes": scenes,
            "syncing": syncing,
            "complete": !syncing,
        }))
    }
```

Add the free function near `hash_scene`:

```rust
/// The grant a scene closes, for every body kind that can be issued as a take.
fn closes_grant(body: &Body) -> Option<Hash32> {
    match body {
        Body::Speech { closes_grant, .. } | Body::Breakout { closes_grant, .. } => {
            Some(*closes_grant)
        }
        Body::Membership { closes_grant, .. } | Body::ViewChange { closes_grant, .. } => {
            *closes_grant
        }
        Body::Genesis { .. } | Body::Grant { .. } => None,
    }
}
```

Check `Body::ViewChange`'s field name in `crates/conch-core/src/types.rs` (line ~342); it is `closes_grant: Option<Hash32>` following `next_roster`. If `BTreeMap`, `Mouth`, or `Body` are not imported at the top of tcp.rs, add them to the existing `use` lists (they are used elsewhere in the file, so they almost certainly are).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p conchd --test lifecycle history_records_name_the_author`
Expected: PASS.

- [ ] **Step 5: Add the HTTP assertion**

In `crates/conchd/tests/http.rs`, test `get_history_matches_cli_protocol` (line ~406) compares the HTTP page with the TCP page. Extend it so the room has one committed take before comparison, and assert the speech record carries `author` on the HTTP side. Read the test first; it uses `daemon.create_genesis("history room")` and `http_get`. After creating the genesis, drive one turn through the TCP client exactly as in Step 1 (`WaitForFloor`, `Speak`, `Yield` via the file's own request helper, or by attaching with `conchd::tcp::{read_frame, write_frame}` as the other tests in that file do), then add:

```rust
    let body: Value = serde_json::from_slice(&response.2).unwrap();
    let speech = body["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["scene"]["body"]["type"] == "speech")
        .expect("the turn committed a speech");
    assert_eq!(speech["author"]["agent"], "agent:test");
```

Adapt `response.2` to however that test already reads the response body. Run: `cargo test -p conchd --test http get_history_matches_cli_protocol`. Expected: PASS.

- [ ] **Step 6: Gate and commit**

```bash
cargo fmt --all
cargo test -p conchd --locked
cargo clippy -p conchd --all-targets -- -D warnings
git add crates/conchd
git commit -m "feat: name the author of every take in served history"
```

---

### Task 2: richer per-room `status`

**Files:**
- Modify: `crates/conchd/src/tcp.rs` (`client_status` ~4475, `operator_room_detail` ~5850 for the shared queue helper)
- Test: `crates/conchd/tests/lifecycle.rs`

**Interfaces:**
- Consumes: `replay.chain.{live_grant, floor_mode, timeout_secs, moderator, roster, consumed_intents}`, `RoomFloor.engine.intents()`, `Inner.room_agents`, `unix_timestamp()`.
- Produces: `status {room}` reply fields `mode`, `timeout_secs`, `holder: {agent, node, grant_hash, since_n, granted_ts} | null`, `queue: [{agent, node, kind, ts}]`, `participants: [agent]`. `who` (Task 6) forwards this reply.

- [ ] **Step 1: Write the failing test**

Append to `crates/conchd/tests/lifecycle.rs`:

```rust
#[tokio::test]
async fn status_reports_holder_queue_and_participants() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "Who",
            conch_core::types::StakePolicy::default(),
            conch_core::types::FloorConfig::stick(300),
        )
        .unwrap();
    let server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let room = ticket.id;
    let status = |req: ClientRequest| request(server.addr(), &req);

    // agent:test takes the floor; a second mouth queues behind it.
    assert!(status(ClientRequest::WaitForFloor { room, timeout_secs: Some(5) }).await.ok);
    let mut second = TcpStream::connect(server.addr()).await.unwrap();
    for message in [
        &ClientRequest::Attach { agent: AgentId::new("agent:second").unwrap() },
        &ClientRequest::RaiseHand { room },
    ] {
        second.write_all(&frame::encode(message).unwrap()).await.unwrap();
        let length = second.read_u32().await.unwrap() as usize;
        let mut payload = vec![0; length];
        second.read_exact(&mut payload).await.unwrap();
        let reply: ClientReply = frame::decode_payload(&payload).unwrap();
        assert!(reply.ok, "{reply:?}");
    }

    let reply = status(ClientRequest::Status { room: Some(room) }).await;
    assert!(reply.ok, "{reply:?}");
    let data = reply.data.unwrap();
    assert_eq!(data["name"], "Who");
    assert_eq!(data["mode"], "stick");
    assert_eq!(data["timeout_secs"], 300);
    assert_eq!(data["holder"]["agent"], "agent:test");
    assert_eq!(data["holder"]["since_n"], 1);
    assert!(data["holder"]["granted_ts"].as_u64().unwrap() > 1_700_000_000);
    assert_eq!(data["holder"]["grant_hash"].as_str().unwrap().len(), 64);
    let queue = data["queue"].as_array().unwrap();
    assert_eq!(queue.len(), 1, "{queue:?}");
    assert_eq!(queue[0]["agent"], "agent:second");
    assert_eq!(queue[0]["kind"], "raise");
    assert_eq!(data["participants"], json!(["agent:second", "agent:test"]));

    assert!(status(ClientRequest::Yield { room }).await.ok);
    // The stick passes to agent:second on its own; wait for that grant.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let replay = daemon.replay(room).unwrap();
            if replay.chain.live_grant.as_ref().is_some_and(|g| g.to.agent.as_str() == "agent:second") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let data = status(ClientRequest::Status { room: Some(room) }).await.data.unwrap();
    assert_eq!(data["holder"]["agent"], "agent:second");
    assert_eq!(data["queue"].as_array().unwrap().len(), 0);
    server.abort();
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p conchd --test lifecycle status_reports_holder`
Expected: FAIL at `data["mode"]` (null).

- [ ] **Step 3: Implement**

Extract the queue computation from `operator_room_detail` into a helper and reuse it. In tcp.rs add, inside `impl Daemon`:

```rust
    /// Unconsumed, uncancelled, unexpired intents in spec §12.3 order.
    fn floor_queue(&self, room: RoomId, replay: &Replay, now: u64) -> Result<Vec<Intent>, DaemonError> {
        let mut queue = self
            .floor(room)?
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .intents()
            .filter(|intent| {
                replay.chain.roster.contains(&intent.node)
                    && !replay.chain.consumed_intents.contains(&intent.id)
                    && now < intent.exp
            })
            .cloned()
            .collect::<Vec<_>>();
        queue.sort_by_key(|intent| (intent.ts, intent.id));
        Ok(queue)
    }
```

Replace the equivalent block in `operator_room_detail` (the `let mut queue = self.floor(room)?...; queue.sort_by_key(...)` lines) with `let queue = self.floor_queue(room, &replay, now)?;` and keep its `.into_iter().enumerate().map(...)` JSON mapping.

Rewrite the per-room arm of `client_status`:

```rust
        if let Some(room) = room {
            let replay = self.replay(room)?;
            let now = unix_timestamp();
            let name = replay.history.first().and_then(|first| match &first.scene.body {
                Body::Genesis { name, .. } => Some(name.clone()),
                _ => None,
            });
            let holder = replay.chain.live_grant.as_ref().map(|grant| {
                let granted_ts = replay
                    .history
                    .iter()
                    .find(|record| record.scene.n == grant.n)
                    .map(|record| record.scene.ts);
                json!({
                    "agent": grant.to.agent,
                    "node": grant.to.node,
                    "grant_hash": grant.hash,
                    "since_n": grant.n,
                    "granted_ts": granted_ts,
                })
            });
            let queue = self
                .floor_queue(room, &replay, now)?
                .into_iter()
                .map(|intent| {
                    json!({
                        "agent": intent.agent,
                        "node": intent.node,
                        "kind": intent.kind,
                        "ts": intent.ts,
                    })
                })
                .collect::<Vec<_>>();
            let mut participants: BTreeSet<AgentId> = self
                .inner
                .room_agents
                .read()
                .expect("room-agent registry lock is not poisoned")
                .get(&room)
                .cloned()
                .unwrap_or_default();
            for record in &replay.history {
                if let Body::Grant { to, .. } = &record.scene.body {
                    participants.insert(to.agent.clone());
                }
            }
            if let Some(moderator) = &replay.chain.moderator {
                participants.insert(moderator.agent.clone());
            }
            return Ok(json!({
                "room": room,
                "name": name,
                "node": self.node_id(),
                "head_n": replay.chain.head_n,
                "head_hash": replay.chain.head_hash,
                "current_term": replay.consensus.current_term,
                "mode": replay.chain.floor_mode,
                "timeout_secs": replay.chain.timeout_secs,
                "holder": holder,
                "queue": queue,
                "participants": participants,
            }));
        }
```

`FloorMode` and `IntentKind` serialize lowercase (`stick`, `raise`), as the existing operator JSON relies on. If `room_agents` only records agents once they join rather than on attach, `agent:test` still appears through its grant; the test's `participants` assertion holds either way.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conchd --test lifecycle status_reports_holder` and `cargo test -p conchd --test http` (the operator detail JSON must be unchanged).
Expected: PASS.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --all
cargo test -p conchd --locked
cargo clippy -p conchd --all-targets -- -D warnings
git add crates/conchd
git commit -m "feat: per-room status reports holder, queue, and participants"
```

---

### Task 3: `timeout_secs` configuration and the 300 s default

**Files:**
- Modify: `crates/conch-core/src/client.rs` (`Membership` variant, line ~56)
- Modify: `crates/conchd/src/tcp.rs` (`ClientRequest::Membership` dispatch ~3074, `client_membership*` ~3420, `create_genesis` ~800, breakout sites ~3601 and ~3731)
- Modify: `crates/conch/src/main.rs` (`create` ~700, `config` ~919, help ~1174 and ~1190)
- Modify: `crates/conch-mcp/src/lib.rs` (`create` ~164, `config` ~280, tool schemas ~767 and ~866)
- Test: `crates/conch/tests/cli_ticket.rs`, `crates/conchd/tests/lifecycle.rs` (finish Task 1's request)

**Interfaces:**
- Produces: `ClientRequest::Membership { room, stake, floor, timeout_secs: Option<u64> }`; `conch create --timeout SECS`, `conch config --timeout SECS`; MCP `create`/`config` argument `timeout`.

- [ ] **Step 1: Write the failing CLI test**

Append to `crates/conch/tests/cli_ticket.rs` (it has `Daemon`, `TempDir`, `Command`, `Value`, `loopback()`):

```rust
#[tokio::test]
async fn create_and_config_set_the_floor_timeout() {
    let data = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let node = format!("tcp://{}", server.addr());
    let conch = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_conch"));
        command
            .arg("--node")
            .arg(&node)
            .args(args)
            .current_dir(cwd.path())
            .env("CONCH_DATA_DIR", data.path());
        command
    };

    let created = conch(&["create", "--name", "Timed", "--timeout", "45"])
        .output()
        .await
        .unwrap();
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    let room = created["id"].as_str().unwrap().to_owned();

    let status = conch(&["--room", &room, "status"]).output().await.unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["timeout_secs"], 45);
    assert_eq!(status["mode"], "stick");

    let configured = conch(&["--room", &room, "config", "--timeout", "90"])
        .output()
        .await
        .unwrap();
    assert!(configured.status.success(), "{}", String::from_utf8_lossy(&configured.stderr));
    let status = conch(&["--room", &room, "status"]).output().await.unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["timeout_secs"], 90);
    assert_eq!(status["mode"], "stick", "mode carried over unchanged");

    let rejected = conch(&["--room", &room, "config", "--timeout", "0"])
        .output()
        .await
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("at least 1"));

    // A room created without --timeout gets the new default.
    let plain = conch(&["create", "--name", "Plain"]).output().await.unwrap();
    let plain: Value = serde_json::from_slice(&plain.stdout).unwrap();
    let room = plain["id"].as_str().unwrap().to_owned();
    let status = conch(&["--room", &room, "status"]).output().await.unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["timeout_secs"], 300);
    server.abort();
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p conch --test cli_ticket create_and_config_set_the_floor_timeout`
Expected: FAIL with "unknown create argument: --timeout".

- [ ] **Step 3: Add the request field**

In `crates/conch-core/src/client.rs`:

```rust
    Membership {
        room: RoomId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stake: Option<StakePolicy>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        floor: Option<FloorConfig>,
        /// Override only the take duration; mode and moderator carry over from
        /// `floor` when given, otherwise from the committed chain state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
```

Fix every constructor of `ClientRequest::Membership` the compiler reports (conch `main.rs` config arm, conch-mcp `config`, conchd tests) by adding `timeout_secs: None` for now, and finish Task 1's test by restoring its `timeout_secs: None` line.

- [ ] **Step 4: Daemon merge and defaults**

In tcp.rs, dispatch (~3074):

```rust
            ClientRequest::Membership {
                room,
                stake,
                floor,
                timeout_secs,
            } => {
                self.client_membership(agent, room, stake, floor, timeout_secs)
                    .await
            }
```

Thread `timeout_secs: Option<u64>` through `client_membership` into `client_membership_from`, and at the top of `client_membership_from`, right after `let replay = self.replay(room)?;` (before the leader forwarding), merge it so the swarm `MembershipReq` keeps carrying a full `FloorConfig`:

```rust
        let floor_config = match (floor_config, timeout_secs) {
            (floor, Some(secs)) => {
                if secs < 1 {
                    return Err(DaemonError::Protocol("timeout_secs must be at least 1"));
                }
                let mut merged = floor.unwrap_or_else(|| floor_config_from_chain(&replay.chain));
                merged.timeout_secs = secs;
                Some(merged)
            }
            (floor, None) => floor,
        };
```

Check how `DaemonError::Protocol` renders to a client (`ClientReply::failure("invalid", ...)` at ~6324) so the CLI prints the message; the test looks for "at least 1".

Defaults: change `FloorConfig::stick(30)` to `FloorConfig::stick(300)` in `create_genesis` (~802). At both breakout sites (~3601 and ~3731) replace `FloorConfig::stick(30)` with `FloorConfig::stick(parent_timeout)` where, before the `spawn_blocking`, you add `let parent_timeout = self.replay(room)?.chain.timeout_secs.unwrap_or(300);` (the first site already has a replay in scope; reuse it).

- [ ] **Step 5: CLI flags**

In `main.rs` `create` (~700): add `let mut timeout_secs = 300_u64;` and the arm

```rust
                        "--timeout" => {
                            timeout_secs = arguments
                                .next()
                                .ok_or("--timeout requires seconds")?
                                .parse::<u64>()
                                .map_err(|error| error.to_string())?;
                            if timeout_secs < 1 {
                                return Err("--timeout must be at least 1".into());
                            }
                        }
```

and build the floor config with it. Read the lines after the `moderator` match (~770) to see how `FloorConfig` is constructed for `Create`; replace the literal `30` with `timeout_secs` (both stick and moderator branches).

In `config` (~919): add `let mut timeout = None;` and

```rust
                        "--timeout" => {
                            let secs = arguments
                                .next()
                                .ok_or("--timeout requires seconds")?
                                .parse::<u64>()
                                .map_err(|error| error.to_string())?;
                            timeout = Some(secs);
                        }
```

Leave the `floor` match's own `timeout_secs: 30` literals alone (the daemon overrides them when `timeout` is given, and `--mode` without `--timeout` keeps today's behaviour). Relax the guard to `if floor.is_none() && stake.is_none() && timeout.is_none()` with the message `"config requires a floor, stake, or timeout change"`, and send `ClientRequest::Membership { room: resolve_room()?, stake, floor, timeout_secs: timeout }`. The daemon merges the timeout into the current floor config. Do not validate `< 1` in the CLI for `config`: the test expects the daemon's "at least 1" wording to surface.

Help (~1174): `"conch create --name NAME [--timeout SECS] [--open | --token HEX | --token-file FILE] [--show-secret]\n ... Takes longer than --timeout (default 300 s) are closed by the leader."` and config: `"conch --room ID config [--mode stick|moderator] [--moderator ID --moderator-node NODE_ID] [--timeout SECS] [--stake-json JSON]"`.

- [ ] **Step 6: MCP arguments**

In `conch-mcp/src/lib.rs` `create`: `let timeout_secs = arguments.optional_u64("timeout").unwrap_or(300);` and use it in both `FloorConfig` branches instead of `30`. Reject `0` with `return Err("timeout must be at least 1".into())`. In `config`: `let timeout_secs = arguments.optional_u64("timeout");`, relax the guard to `floor.is_none() && stake.is_none() && timeout_secs.is_none()`, and pass `timeout_secs`. Tool schemas: add `"timeout": { "type": "integer", "minimum": 1, "default": 300, "description": "Seconds a holder may keep the floor before the leader closes the take" }` to `create`, and `"timeout": { "type": "integer", "minimum": 1 }` to `config`.

- [ ] **Step 7: Run the tests**

Run: `cargo test -p conch --test cli_ticket create_and_config_set_the_floor_timeout` and `cargo test -p conchd --test lifecycle`.
Expected: PASS.

- [ ] **Step 8: Gate and commit**

```bash
cargo fmt --all
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
git add crates
git commit -m "feat: --timeout on create and config; new rooms default to 300 s"
```

---

### Task 4: leader enforces the floor timeout

**Files:**
- Modify: `crates/conchd/src/tcp.rs` (`client_yank_from` ~3337, `Inner` ~120, `start`/`serve`/`start_tls`/`serve_tls` ~1030)
- Test: `crates/conchd/tests/features.rs`

**Interfaces:**
- Consumes: `close_live_grant` (extracted here), `request_remote_freeze`, `commit_singleton_body`, `floor_queue` not needed.
- Produces: `Daemon::spawn_floor_timeouts(&self)` (idempotent), `async fn enforce_floor_timeouts(&self)`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/conchd/tests/features.rs` (it has `attach`, `request(&mut stream, ClientRequest)`, `loopback`, `network_test_guard`, `Body`, `FloorConfig`):

```rust
#[tokio::test]
async fn leader_closes_an_overdue_local_take_with_its_appended_text() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket("Timed", StakePolicy::default(), FloorConfig::stick(1))
        .unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let mut holder = attach(server.addr(), "agent:slow").await;
    let granted = request(
        &mut holder,
        ClientRequest::WaitForFloor {
            room: ticket.id,
            timeout_secs: Some(5),
        },
    )
    .await;
    assert!(granted.ok, "{granted:?}");
    let spoke = request(
        &mut holder,
        ClientRequest::Speak {
            room: ticket.id,
            text: "half a thought".into(),
            request_id: "00000000000000000000000000000031".into(),
        },
    )
    .await;
    assert!(spoke.ok, "{spoke:?}");
    // The holder never yields. Within the timeout plus one tick the leader closes it.
    let closed = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let replay = daemon.replay(ticket.id).unwrap();
            if replay.chain.live_grant.is_none() {
                return replay.history;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the overdue grant was closed");
    assert!(matches!(
        &closed.last().unwrap().scene.body,
        Body::Speech { text, .. } if text == "half a thought"
    ));
    // Nothing else is committed afterwards: a vacant floor has nothing to time out.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(daemon.replay(ticket.id).unwrap().history.len(), closed.len());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leader_empty_closes_an_overdue_remote_holder_it_cannot_reach() {
    let _network_test = network_test_guard().await;
    let source_data = TempDir::new().unwrap();
    let holder_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let holder = Daemon::open(holder_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let holder_server = holder.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket("Remote timeout", StakePolicy::default(), FloorConfig::stick(2))
        .unwrap();
    holder
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();
    let mut writer = attach(holder_server.addr(), "agent:remote").await;
    let granted = request(
        &mut writer,
        ClientRequest::WaitForFloor {
            room: ticket.id,
            timeout_secs: Some(10),
        },
    )
    .await;
    assert!(granted.ok, "{granted:?}");
    // The holder's daemon disappears without yielding.
    holder_server.abort();
    drop(writer);
    let history = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let replay = source.replay(ticket.id).unwrap();
            if replay.chain.live_grant.is_none() {
                return replay.history;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the leader empty-closed the unreachable holder");
    assert!(matches!(
        &history.last().unwrap().scene.body,
        Body::Speech { text, blobs, .. } if text.is_empty() && blobs.is_empty()
    ));
}
```

Note for the second test: with two stakers the leader needs a majority to commit; the source node alone is one of two. Read `two_daemon_staker_wrap_uses_network_votes_and_certs` and `remaining_majority_elects_successor_after_leader_stops` in the same file to see how commits proceed when a peer is gone. If a two-node roster cannot commit with one node down (spec §11: 1 of 2 cannot wrap), change the test to three daemons (source, a second staker that stays up, and the holder that goes away) so the remaining two form a majority; the assertions stay the same. `RunningServer::abort` only stops the listener task; if the holder daemon still answers `freeze` over an existing peer connection, additionally drop the `holder` `Daemon` value after aborting so its connections close.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p conchd --test features leader_`
Expected: both FAIL on the `timeout(...)` expectation (the grant stays live).

- [ ] **Step 3: Extract `close_live_grant` from the yank path**

In tcp.rs, `client_yank_from`: keep everything through `let _mutation = floor.mutation.lock().await; let replay = self.replay(room)?; self.require_moderator_mouth(&replay, &from)?;` and replace the remainder (from `let grant = replay.chain.live_grant.ok_or(FloorError::NoGrant)?;` to `Ok(json!({ "ok": true, "closes_grant": grant.hash }))`) with `self.close_live_grant(room, &floor, &replay).await`. Add:

```rust
    /// Freeze and commit the live take (spec §12.1). The caller holds
    /// `floor.mutation` and has already decided this node may close it: a
    /// moderator yank, or the leader's floor timeout.
    async fn close_live_grant(
        &self,
        room: RoomId,
        floor: &Arc<RoomFloor>,
        replay: &Replay,
    ) -> Result<Value, DaemonError> {
        let grant = replay.chain.live_grant.clone().ok_or(FloorError::NoGrant)?;
        // ... the body previously in client_yank_from, verbatim, using `grant` ...
        Ok(json!({ "ok": true, "closes_grant": grant.hash }))
    }
```

Move the code, do not retype it: `ensure_network_leader`, `broadcast_heartbeat`, the local freeze, `request_remote_freeze`, and the `commit_singleton_body` loop are unchanged. Run `cargo test -p conchd --test features moderator_yank` to confirm the yank tests still pass before continuing.

- [ ] **Step 4: The ticker**

Add to `Inner`: `floor_timeouts_started: AtomicBool,` (initialise `AtomicBool::new(false)` in `Daemon::open`; import `std::sync::atomic::{AtomicBool, Ordering}`). Add to `impl Daemon`:

```rust
    /// Start the leader's floor-timeout ticker once per process (spec §12.1).
    pub fn spawn_floor_timeouts(&self) {
        if self
            .inner
            .floor_timeouts_started
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let daemon = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                daemon.enforce_floor_timeouts().await;
            }
        });
    }

    /// One pass over every loaded room: close the live grant of any room this node
    /// leads whose take is older than the room's `timeout_secs`.
    async fn enforce_floor_timeouts(&self) {
        let rooms: Vec<RoomId> = self
            .inner
            .rooms
            .read()
            .expect("room registry lock is not poisoned")
            .keys()
            .copied()
            .collect();
        for room in rooms {
            let Ok(replay) = self.replay(room) else { continue };
            let Some(grant) = replay.chain.live_grant.clone() else { continue };
            let leads = replay.chain.roster.len() <= 1
                || (replay.consensus.role == ConsensusRole::Leader
                    && replay.consensus.leader_id == Some(self.node_id()));
            if !leads {
                continue;
            }
            let Some(timeout_secs) = replay.chain.timeout_secs else { continue };
            let Some(granted_ts) = replay
                .history
                .iter()
                .find(|record| record.scene.n == grant.n)
                .map(|record| record.scene.ts)
            else {
                continue;
            };
            let age = unix_timestamp().saturating_sub(granted_ts);
            if age < timeout_secs {
                continue;
            }
            let Ok(floor) = self.floor(room) else { continue };
            // A close already in flight (a yank, or the previous tick) keeps the lock.
            let Ok(_mutation) = floor.mutation.try_lock() else { continue };
            let Ok(replay) = self.replay(room) else { continue };
            if replay.chain.live_grant.as_ref().map(|live| live.hash) != Some(grant.hash) {
                continue;
            }
            match self.close_live_grant(room, &floor, &replay).await {
                Ok(_) => {
                    let empty = self
                        .replay(room)
                        .ok()
                        .and_then(|replay| replay.history.last().cloned())
                        .is_some_and(|record| {
                            matches!(&record.scene.body, Body::Speech { text, blobs, .. } if text.is_empty() && blobs.is_empty())
                        });
                    eprintln!(
                        "conchd: room {room}: floor timeout after {age}s, holder {}@{}, empty={empty}",
                        grant.to.agent, grant.to.node
                    );
                }
                // The holder acknowledged the freeze and is still CLOSING; try again next tick.
                Err(DaemonError::MutationUnavailable) => {}
                Err(error) => eprintln!("conchd: room {room}: floor timeout close failed: {error}"),
            }
        }
    }
```

Use whatever logging the file already uses for daemon-side messages if it is not `eprintln!` (search for how `serve_listener` reports errors) so output lands in `conchd.log`. Call `self.spawn_floor_timeouts();` as the first line of `start`, `serve`, `start_tls`, and `serve_tls`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p conchd --test features leader_` then the whole file: `cargo test -p conchd --test features`.
Expected: PASS. If the remote test's empty close takes longer than expected, remember the sequence is: age ≥ 2 s, then `request_remote_freeze` waits `FREEZE_WAIT` (5 s) on a connection failure, then commit; 20 s is generous.

- [ ] **Step 6: Gate and commit**

```bash
cargo fmt --all
cargo test -p conchd --locked
cargo clippy -p conchd --all-targets -- -D warnings
git add crates/conchd
git commit -m "feat: leader closes takes that outlive the room's floor timeout"
```

---

### Task 5: mention matching and event flattening (pure)

**Files:**
- Create: `crates/conch-mcp/src/events.rs`
- Modify: `crates/conch-mcp/src/lib.rs` (add `pub mod events;`)
- Test: unit tests inside `events.rs`

**Interfaces:**
- Produces: `pub fn mentions(text: &str, agent: &AgentId) -> bool`; `pub fn flatten(records: &[Value], you: &Mouth) -> Vec<Value>`; `pub fn last_height(records: &[Value]) -> Option<u64>`. Records are the JSON objects served by Task 1 (`scene`, `commit_proof`, optional `author`).

- [ ] **Step 1: Write the failing tests**

Create `crates/conch-mcp/src/events.rs` with only the tests module first:

```rust
//! Agent-facing view of committed scenes: which takes mention me, when the floor
//! is mine, who holds it now. Pure functions over the JSON the daemon serves.

use conch_core::types::{AgentId, Mouth};
use serde_json::{json, Value};

#[cfg(test)]
mod tests {
    use super::*;
    use conch_core::types::NodeId;

    fn agent(id: &str) -> AgentId {
        AgentId::new(id).unwrap()
    }

    #[test]
    fn mentions_match_full_id_short_name_and_everyone_with_word_bounds() {
        let me = agent("agent:claude");
        for text in [
            "@agent:claude ping",
            "hey @claude, ping",
            "@Claude?",
            "(@claude)",
            "all hands @all",
            "@everyone look",
            "line\n@claude",
        ] {
            assert!(mentions(text, &me), "{text:?}");
        }
        for text in [
            "email@claude.ai",
            "@claudette",
            "@claude_2",
            "@claude.dev",
            "claude without at",
            "@agent:codex",
            "",
        ] {
            assert!(!mentions(text, &me), "{text:?}");
        }
        // An id without a colon has only its full form.
        assert!(mentions("@local hi", &agent("local")));
        assert!(!mentions("@loc hi", &agent("local")));
    }

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    fn record(n: u64, body: Value, author: Option<(&str, u8)>) -> Value {
        let mut record = json!({
            "scene": { "n": n, "ts": 1_700_000_000 + n, "body": body },
            "commit_proof": {}
        });
        if let Some((agent, node)) = author {
            record["author"] = json!({ "agent": agent, "node": node_json(node) });
        }
        record
    }

    fn node_json(byte: u8) -> Value {
        serde_json::to_value(node(byte)).unwrap()
    }

    fn me() -> Mouth {
        Mouth { agent: agent("agent:claude"), node: node(1) }
    }

    #[test]
    fn every_scene_kind_flattens_to_at_most_one_event() {
        let records = vec![
            record(0, json!({ "type": "genesis", "name": "r" }), None),
            record(1, json!({ "type": "grant", "to": { "agent": "agent:codex", "node": node_json(2) }, "reason": "queue", "intent_id": "00" }), None),
            record(2, json!({ "type": "speech", "closes_grant": "aa", "text": "hi @claude" }), Some(("agent:codex", 2))),
            record(3, json!({ "type": "grant", "to": { "agent": "agent:claude", "node": node_json(1) }, "reason": "queue", "intent_id": "01" }), None),
            record(4, json!({ "type": "speech", "closes_grant": "bb", "text": "@claude talking to myself" }), Some(("agent:claude", 1))),
            record(5, json!({ "type": "grant", "to": { "agent": "agent:codex", "node": node_json(2) }, "reason": "queue", "intent_id": "02" }), None),
            record(6, json!({ "type": "speech", "closes_grant": "cc", "text": "" }), Some(("agent:codex", 2))),
            record(7, json!({ "type": "view-change", "add": [node_json(3)], "remove": [], "next_roster": [] }), None),
            record(8, json!({ "type": "membership", "stake": {}, "floor": { "mode": "stick", "timeout_secs": 300 } }), None),
            record(9, json!({ "type": "grant", "to": { "agent": "agent:codex", "node": node_json(2) }, "reason": "queue", "intent_id": "03" }), None),
            record(10, json!({ "type": "membership", "closes_grant": "dd", "stake": {}, "floor": { "mode": "stick", "timeout_secs": 60 } }), Some(("agent:codex", 2))),
        ];
        let events = flatten(&records, &me());
        let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            ["floor", "mention", "granted", "speech", "floor", "speech", "roster", "config", "floor", "floor"]
        );
        assert_eq!(events[0]["holder"]["agent"], "agent:codex");
        assert_eq!(events[1]["author"]["agent"], "agent:codex");
        assert_eq!(events[1]["text"], "hi @claude");
        assert_eq!(events[1]["grant_hash"], "aa");
        // `granted` carries the grant scene's own hash, computed from the scene JSON.
        assert_eq!(events[2]["grant_hash"].as_str().unwrap().len(), 64);
        assert_eq!(events[3]["author"]["agent"], "agent:claude", "own take is speech, never mention");
        assert_eq!(events[5]["empty"], true);
        assert_eq!(events[6]["added"], json!([node_json(3)]));
        assert_eq!(events[7]["timeout_secs"], 300);
        // A take closed by a non-speech scene vacates the floor and names its author.
        assert_eq!(events[9]["holder"], Value::Null);
        assert_eq!(events[9]["author"]["agent"], "agent:codex");
        for (event, n) in events.iter().zip([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]) {
            assert_eq!(event["n"], n);
            assert_eq!(event["ts"], 1_700_000_000 + n);
        }
        assert_eq!(last_height(&records), Some(10));
        assert_eq!(last_height(&[]), None);
    }
}
```

A `granted` event carries the grant scene's own hash. Records carry no hash, so `flatten` computes it with `conch_core::encoding::scene_hash(&record["scene"])` wrapped in `Hash32::from_bytes`; the test only checks its length.

- [ ] **Step 2: Run the tests to verify they fail**

Add `pub mod events;` near the top of `crates/conch-mcp/src/lib.rs`. Run: `cargo test -p conch-mcp events`
Expected: compile error, `mentions`/`flatten`/`last_height` not found.

- [ ] **Step 3: Implement**

Above the tests module in `events.rs`:

```rust
use conch_core::{encoding::scene_hash, types::Hash32};

/// Does `text` address `agent`? `@` plus the full id, its short name after the
/// first colon, or `all`/`everyone`; case-insensitive; the `@` starts the text or
/// follows whitespace or punctuation; the name is not followed by an id character.
pub fn mentions(text: &str, agent: &AgentId) -> bool {
    let full = agent.as_str().to_ascii_lowercase();
    let short = full.split_once(':').map(|(_, short)| short.to_owned());
    let lower = text.to_ascii_lowercase();
    let mut names: Vec<&str> = vec![full.as_str(), "all", "everyone"];
    if let Some(short) = short.as_deref() {
        names.push(short);
    }
    let bytes = lower.as_bytes();
    for (at, _) in lower.match_indices('@') {
        let preceded_ok = at == 0
            || bytes[at - 1].is_ascii_whitespace()
            || bytes[at - 1].is_ascii_punctuation();
        if !preceded_ok {
            continue;
        }
        let rest = &lower[at + 1..];
        for name in &names {
            if let Some(after) = rest.strip_prefix(name) {
                let boundary = after.chars().next().is_none_or(|c| !is_id_char(c));
                if boundary {
                    return true;
                }
            }
        }
    }
    false
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.')
}

/// The last committed height in a page, whether or not it produced an event.
pub fn last_height(records: &[Value]) -> Option<u64> {
    records.last().and_then(|record| record["scene"]["n"].as_u64())
}

/// One event per scene that concerns an agent; genesis produces none.
pub fn flatten(records: &[Value], you: &Mouth) -> Vec<Value> {
    let you_json = serde_json::to_value(you).expect("mouth is serializable");
    records
        .iter()
        .filter_map(|record| {
            let scene = &record["scene"];
            let body = &scene["body"];
            let n = scene["n"].clone();
            let ts = scene["ts"].clone();
            let author = record.get("author").cloned();
            let base = |kind: &str| json!({ "kind": kind, "n": n, "ts": ts });
            let mut event = match body["type"].as_str()? {
                "genesis" => return None,
                "grant" => {
                    if body["to"] == you_json {
                        let hash = Hash32::from_bytes(scene_hash(scene));
                        let mut event = base("granted");
                        event["grant_hash"] = json!(hash);
                        event
                    } else {
                        let mut event = base("floor");
                        event["holder"] = body["to"].clone();
                        event
                    }
                }
                "speech" => {
                    let text = body["text"].as_str().unwrap_or("").to_owned();
                    let mine = author.as_ref().is_some_and(|author| *author == you_json);
                    let mentioned = !mine && mentions(&text, &you.agent);
                    let mut event = base(if mentioned { "mention" } else { "speech" });
                    event["author"] = author.clone().unwrap_or(Value::Null);
                    event["text"] = json!(text);
                    if mentioned {
                        event["grant_hash"] = body["closes_grant"].clone();
                    } else {
                        event["empty"] = json!(text.is_empty() && body["blobs"].as_array().is_none_or(|b| b.is_empty()));
                    }
                    event
                }
                "view-change" if body.get("closes_grant").is_none() => {
                    let mut event = base("roster");
                    event["added"] = body["add"].clone();
                    event["removed"] = body["remove"].clone();
                    event
                }
                "membership" if body.get("closes_grant").is_none() => {
                    let mut event = base("config");
                    event["mode"] = body["floor"]["mode"].clone();
                    event["timeout_secs"] = body["floor"]["timeout_secs"].clone();
                    event
                }
                // breakout, or a membership/view-change issued as a take: the floor is vacant.
                _ => {
                    let mut event = base("floor");
                    event["holder"] = Value::Null;
                    event["author"] = author.clone().unwrap_or(Value::Null);
                    event
                }
            };
            if event["author"].is_null() {
                event.as_object_mut().map(|object| object.remove("author"));
            }
            Some(event)
        })
        .collect()
}
```

Check `scene_hash`'s exact signature in `crates/conch-core/src/encoding.rs` (line ~22: `pub fn scene_hash(scene_json: &Value) -> [u8; 32]`) and that `conch-mcp` depends on `conch-core` (it does). A `view-change` or `membership` that closes a take falls through to the last arm and is reported as `floor` with the floor vacant.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conch-mcp events`
Expected: PASS.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --all
cargo clippy -p conch-mcp --all-targets -- -D warnings
git add crates/conch-mcp
git commit -m "feat: mention matching and event flattening for MCP listen"
```

---

### Task 6: `say`, `listen`, `who`; retire `raise_hand`

**Files:**
- Modify: `crates/conch-mcp/src/lib.rs` (`handle_message` instructions ~85, `call_tool` ~110, `prepare` ~268, `tool_definitions` ~760)
- Test: `crates/conch/tests/cli_mcp.rs`

**Interfaces:**
- Consumes: Task 5 `events::{flatten, last_height}`, Task 1 `author`, Task 2 status fields, `derived_request_id`, `MAX_HISTORY_WAIT_SECS`.
- Produces: tools `say {room?, text, timeout?}` → `{n, grant_hash, author}`; `listen {room?, after, timeout?}` → `{events, height, timed_out}`; `who {room?}` → status reply plus `you` and `head`. `raise_hand` no longer exists.

- [ ] **Step 1: Update the concurrency test and write the two-agent test**

In `crates/conch/tests/cli_mcp.rs`, `ping_stays_responsive_while_waiting_then_mcp_completes_the_turn` calls `raise_hand` (id 17) to produce scene 5. Replace that call with a `say` sent asynchronously, and assert the wait returns the grant at `n == 5`:

```rust
    holder
        .send(tool_call(17, "say", json!({ "text": "holder speaks again\n", "timeout": 5 })))
        .await;
    let committed = participant.receive().await;
    assert_eq!(committed["id"], 15);
    assert_eq!(committed["result"]["isError"], false);
    let page: Value =
        serde_json::from_str(committed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(page["timed_out"], false);
    assert_eq!(page["scenes"][0]["scene"]["n"], 5);
    let said = holder.receive().await;
    assert_eq!(said["id"], 17);
    assert_eq!(said["result"]["isError"], false, "{said}");
    assert_eq!(said["result"]["structuredContent"]["n"], 6);
    assert_eq!(said["result"]["structuredContent"]["author"]["agent"], "agent:holder");
```

Also in `conch_mcp_serves_newline_delimited_json_rpc`, add:

```rust
    let names: Vec<&str> = replies[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in ["say", "listen", "who"] {
        assert!(names.contains(&expected), "{names:?}");
    }
    assert!(!names.contains(&"raise_hand"), "{names:?}");
```

Then append the two-agent test:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_address_each_other_with_mentions_through_listen_and_say() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket("Mentions", StakePolicy::default(), FloorConfig::stick(300))
        .unwrap();
    let daemon_server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let node = format!("tcp://{}", daemon_server.addr());
    let room = ticket.id.to_string();
    let binary = env!("CARGO_BIN_EXE_conch");
    let mut claude = McpProcess::spawn(binary, &node, &room, "agent:claude", data.path()).await;
    let mut codex = McpProcess::spawn(binary, &node, &room, "agent:codex", data.path()).await;

    let who = claude.call(tool_call(1, "who", json!({}))).await;
    assert_eq!(who["result"]["isError"], false, "{who}");
    let who = &who["result"]["structuredContent"];
    assert_eq!(who["name"], "Mentions");
    assert_eq!(who["holder"], Value::Null);
    assert_eq!(who["you"]["agent"], "agent:claude");
    assert_eq!(who["head"], 0);

    // codex listens from height 0 while claude speaks.
    codex
        .send(tool_call(10, "listen", json!({ "after": 0, "timeout": 10 })))
        .await;
    let said = claude
        .call(tool_call(2, "say", json!({ "text": "@codex ping", "timeout": 5 })))
        .await;
    assert_eq!(said["result"]["isError"], false, "{said}");
    assert_eq!(said["result"]["structuredContent"]["author"]["agent"], "agent:claude");
    let heard = codex.receive().await;
    assert_eq!(heard["id"], 10);
    let page = &heard["result"]["structuredContent"];
    assert_eq!(page["timed_out"], false);
    let events = page["events"].as_array().unwrap();
    // The grant to claude is a floor event; the take is a mention of codex.
    assert!(events.iter().any(|e| e["kind"] == "floor" && e["holder"]["agent"] == "agent:claude"), "{events:?}");
    let mention = events.iter().find(|e| e["kind"] == "mention").expect("mention event");
    assert_eq!(mention["author"]["agent"], "agent:claude");
    assert_eq!(mention["text"], "@codex ping");
    assert_eq!(page["height"], events.last().unwrap()["n"]);

    // codex answers; claude hears it as a mention.
    let height = page["height"].as_u64().unwrap();
    let answered = codex
        .call(tool_call(11, "say", json!({ "text": "@claude pong", "timeout": 5 })))
        .await;
    assert_eq!(answered["result"]["isError"], false, "{answered}");
    let heard = claude
        .call(tool_call(3, "listen", json!({ "after": height, "timeout": 5 })))
        .await;
    let events = heard["result"]["structuredContent"]["events"].as_array().unwrap().clone();
    let mention = events.iter().find(|e| e["kind"] == "mention").expect("mention event");
    assert_eq!(mention["author"]["agent"], "agent:codex");
    assert_eq!(mention["text"], "@claude pong");

    // A quiet room: listen times out with no events and the height it examined.
    let quiet = claude
        .call(tool_call(4, "listen", json!({ "after": height + 2, "timeout": 1 })))
        .await;
    let page = &quiet["result"]["structuredContent"];
    assert_eq!(page["timed_out"], true);
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["height"], height + 2);

    claude.shutdown().await;
    codex.shutdown().await;
}
```

`say` returns before the listener's `wait_for_history` necessarily has: the daemon delivers the page when the closing speech commits, which `say` also waits for, so `codex.receive()` after `claude.call(say)` is deterministic.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p conch --test cli_mcp`
Expected: the tools-list test fails (`say` missing), the mention test fails with "unknown Conch tool: who".

- [ ] **Step 3: Implement the composite tools**

In `call_tool`, before `let prepared = self.prepare(name, &arguments).await;`:

```rust
        let composite = match name {
            "say" => Some(self.say(&arguments).await),
            "listen" => Some(self.listen(&arguments).await),
            "who" => Some(self.who(&arguments).await),
            _ => None,
        };
        if let Some(result) = composite {
            return Ok(match result {
                Ok(data) => tool_success(data),
                Err((code, message)) => tool_error(&code, &message),
            });
        }
```

Extract the existing success-building block (the `let structured = ...; Ok(json!({ "content": ..., "structuredContent": ..., "isError": false }))`) into `fn tool_success(data: Value) -> Value` and use it in both places.

Add the helpers to `impl Server`:

```rust
    /// One daemon round trip; an error reply becomes `(code, message)`.
    async fn ask(&self, request: ClientRequest) -> Result<Value, (String, String)> {
        match self.send(request, None).await {
            Ok(reply) if reply.ok => Ok(reply.data.unwrap_or(Value::Null)),
            Ok(reply) => Err(reply.error.map_or_else(
                || ("invalid".to_owned(), "daemon returned an unspecified error".to_owned()),
                |error| (error.code, error.message),
            )),
            Err(error) => Err(("unavailable".to_owned(), error)),
        }
    }

    fn you(&self, node: &Value) -> Value {
        json!({ "agent": self.agent, "node": node })
    }

    async fn who(&self, arguments: &Arguments) -> Result<Value, (String, String)> {
        let room = arguments.room().map_err(|e| ("invalid".to_owned(), e))?;
        let mut status = self.ask(ClientRequest::Status { room: Some(room) }).await?;
        let node = status["node"].clone();
        let head = status["head_n"].clone();
        if let Some(object) = status.as_object_mut() {
            object.insert("head".into(), head);
            object.insert("you".into(), self.you(&node));
        }
        Ok(status)
    }

    async fn listen(&self, arguments: &Arguments) -> Result<Value, (String, String)> {
        let invalid = |message: String| ("invalid".to_owned(), message);
        let room = arguments.room().map_err(invalid)?;
        let after = arguments
            .optional_u64("after")
            .ok_or_else(|| invalid("listen requires after".into()))?;
        let timeout_secs = arguments.optional_u64("timeout").unwrap_or(60);
        if timeout_secs > MAX_HISTORY_WAIT_SECS {
            return Err(invalid(format!("timeout must be at most {MAX_HISTORY_WAIT_SECS} seconds")));
        }
        let page = self
            .ask(ClientRequest::WaitForHistory { room, after_n: after, timeout_secs: Some(timeout_secs) })
            .await?;
        let records = page["scenes"].as_array().cloned().unwrap_or_default();
        let node = self.local_node(room).await?;
        let you = Mouth { agent: self.agent.clone(), node };
        Ok(json!({
            "events": events::flatten(&records, &you),
            "height": events::last_height(&records).unwrap_or(after),
            "timed_out": page["timed_out"].as_bool().unwrap_or(false),
        }))
    }

    /// The daemon's node id, needed to tell our own grants and takes from others'.
    async fn local_node(&self, room: RoomId) -> Result<NodeId, (String, String)> {
        let status = self.ask(ClientRequest::Status { room: Some(room) }).await?;
        serde_json::from_value(status["node"].clone())
            .map_err(|error| ("unavailable".to_owned(), format!("status omitted node: {error}")))
    }

    async fn say(&self, arguments: &Arguments) -> Result<Value, (String, String)> {
        let invalid = |message: String| ("invalid".to_owned(), message);
        let room = arguments.room().map_err(invalid)?;
        let text = arguments.string("text").map_err(invalid)?;
        let timeout_secs = arguments.optional_u64("timeout").unwrap_or(300).min(300);
        let grant = self
            .ask(ClientRequest::WaitForFloor { room, timeout_secs: Some(timeout_secs) })
            .await?;
        let grant_n = grant["n"].as_u64().unwrap_or(0);
        let grant_hash = Hash32::from_bytes(scene_hash(&grant));
        let request_id = derived_request_id(&room, &self.agent, &text);
        let speak = ClientRequest::Speak { room, text: text.clone(), request_id };
        let spoke = match self.ask(speak.clone()).await {
            Err((code, _)) if code == "invalid" => self.ask(speak).await,
            other => other,
        };
        // Whatever happened to the append, never leave the grant held.
        let yielded = self.ask(ClientRequest::Yield { room }).await;
        spoke?;
        yielded?;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut after = grant_n;
        loop {
            let page = self
                .ask(ClientRequest::WaitForHistory { room, after_n: after, timeout_secs: Some(10) })
                .await?;
            let records = page["scenes"].as_array().cloned().unwrap_or_default();
            if let Some(record) = records.iter().find(|record| {
                record["scene"]["body"]["closes_grant"] == json!(grant_hash)
            }) {
                return Ok(json!({
                    "n": record["scene"]["n"],
                    "grant_hash": grant_hash,
                    "author": record.get("author").cloned().unwrap_or(Value::Null),
                }));
            }
            after = events::last_height(&records).unwrap_or(after);
            if std::time::Instant::now() >= deadline {
                return Err(("unavailable".to_owned(), "the take was accepted but its closing speech has not committed within 60 s; check history".to_owned()));
            }
        }
    }
```

Imports needed at the top of lib.rs: `conch_core::encoding::scene_hash`, `Hash32`, `Mouth`, `NodeId` (check the existing `use conch_core::{...}` block), `std::time::Duration`. `ClientRequest` must derive `Clone` for `speak.clone()`; it does (check `#[derive]` on the enum in client.rs, add `Clone` if absent, which is not a wire change).

Remove `"raise_hand" => ...` from `prepare` and the `raise_hand` entry from `tool_definitions`. Add definitions:

```rust
        tool(
            "say",
            "Take one turn: wait for the floor, speak the whole text, yield, and return the committed scene",
            object_schema(
                room_properties(json!({
                    "text": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 0, "maximum": 300, "default": 300, "description": "Seconds to wait for the floor" }
                })),
                &["text"],
            ),
        ),
        tool(
            "listen",
            "Wait for committed scenes after a height and return them as events: mention, speech, granted, floor, roster, config",
            object_schema(
                room_properties(json!({
                    "after": { "type": "integer", "minimum": 0, "description": "Last committed height already processed" },
                    "timeout": { "type": "integer", "minimum": 0, "maximum": 300, "default": 60 }
                })),
                &["after"],
            ),
        ),
        tool(
            "who",
            "Room name, head, floor mode and timeout, current holder, queue, participants, and your own mouth",
            object_schema(room_properties(json!({})), &[]),
        ),
```

Update the `initialize` instructions string to: `"Join a room, call who, then loop listen from the last height. Answer a mention or take a granted floor with say. Do not speak without a grant. Unless the operator gave a terminal condition, remain present by calling listen again after every page."`

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conch --test cli_mcp`
Expected: PASS (three tests).

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --all
cargo test -p conch-mcp -p conch --locked
cargo clippy -p conch-mcp -p conch --all-targets -- -D warnings
git add crates/conch-mcp crates/conch/tests/cli_mcp.rs
git commit -m "feat: say, listen, and who MCP tools; raise_hand retired"
```

---

### Task 7: console shows authors and the floor holder

**Files:**
- Modify: `ui/app.js` (`describe` ~410, `renderScene` ~390, `renderRoomDetail` ~262), `ui/index.html` (room-meta ~89)
- Test: `crates/conchd/tests/http.rs` (`ui_html_is_embedded_and_served_at_root_and_ui`)

**Interfaces:**
- Consumes: Task 1 `record.author`; the operator detail's `floor.holder` (already served).

- [ ] **Step 1: Write the failing assertion**

In `ui_html_is_embedded_and_served_at_root_and_ui` (http.rs ~563), after the existing assertions on the served app, add:

```rust
    assert!(app.contains("Empty take"), "speech cards label empty takes");
    assert!(!app.contains("Wrapped take"), "speech cards are titled by author");
    assert!(app.contains("id=\"floor-holder\""), "room header shows the holder");
```

Read the test to see which variable holds the served `app.js`/`index.html` text; the existing `history.pushState` assertion shows the pattern. Run: `cargo test -p conchd --test http ui_html_is_embedded`. Expected: FAIL on the first new assertion.

- [ ] **Step 2: Implement**

`ui/index.html`, in `.room-meta`, add after the floor-mode block:

```html
              <div><span>Floor</span><strong id="floor-holder">—</strong></div>
```

`ui/app.js`:

1. In the `el` map (near the top, where `floorMode` is looked up) add `floorHolder: document.getElementById("floor-holder"),` following the existing style.
2. In `renderRoomDetail`, after `el.floorMode.textContent = ...`, add:

```js
  const holderMouth = state.detail.floor?.holder || null;
  el.floorHolder.textContent = holderMouth ? holderMouth.agent : "vacant";
  el.floorHolder.title = holderMouth ? `on node ${short(holderMouth.node)}` : "";
```

3. Change `describe(body)` to `describe(body, author)` and the speech case to:

```js
    case "speech": return { title: author?.agent || "Unknown author", kind: "Speech", copy: body.text || "Empty take" };
```

4. In `renderScene` (where `describe(body)` is called, ~line 395), pass the record's author: `const rendered = describe(body, record.author);`. Read the function to find the record variable name (`record` is destructured as `{ scene, commit_proof: proof }` at ~390).
5. In `index.html` line ~118 the placeholder copy "The first wrapped take will appear here." may stay.

- [ ] **Step 3: Run the test**

Run: `cargo test -p conchd --test http ui_html_is_embedded`
Expected: PASS. Then open the console against a temp daemon to eyeball it: `CONCH_DATA_DIR=$(mktemp -d) CONCH_DEFAULT_TCP=127.0.0.1:27421 CONCH_DEFAULT_HTTP=127.0.0.1:27420 cargo run -p conch -- up`, create a room and take a turn with the same env, open `http://127.0.0.1:27420/`, then `conch down` with the same env. Never use the default ports.

- [ ] **Step 4: Gate and commit**

```bash
cargo fmt --all
cargo test -p conchd --test http
git add ui crates/conchd/tests/http.rs
git commit -m "feat: console titles takes by author and shows the floor holder"
```

---

### Task 8: skill, remedies, and docs

**Files:**
- Modify: `skills/join-room/SKILL.md`, `crates/conch/src/remedy.rs`, `crates/conch/src/setup.rs` (tests), `README.md`, `integrations/README.md`
- Test: unit tests in `remedy.rs` and `setup.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/conch/src/setup.rs` tests module add:

```rust
    #[test]
    fn embedded_skill_teaches_the_listen_say_loop() {
        let text = skill_text("1.2.2");
        for word in ["`who`", "`listen`", "`say`", "@codex", "`wait_for_floor`"] {
            assert!(text.contains(word), "skill lacks {word}");
        }
        assert!(!text.contains("raise_hand"), "raise_hand is retired from MCP");
    }
```

In `crates/conch/src/remedy.rs` tests, change the `no_grant` expectation to:

```rust
        assert_eq!(
            for_code("no_grant", "speak"),
            Some("wait for the floor first: `conch wait-for-floor` (or the floor timed out; check `conch status`)")
        );
```

Run: `cargo test -p conch --lib skill_teaches` and `cargo test -p conch --lib remedy`. Expected: both FAIL.

- [ ] **Step 2: Remedy**

In `remedy.rs`:

```rust
        ("no_grant", _) => {
            "wait for the floor first: `conch wait-for-floor` (or the floor timed out; check `conch status`)"
        }
```

- [ ] **Step 3: Rewrite the skill's MCP section**

Replace the `## MCP mode` section of `skills/join-room/SKILL.md` (from its heading up to `## Participation lifecycle`) with:

```markdown
## MCP mode

Use the `join`, `who`, `listen`, `say`, `history`, `wait_for_floor`, `speak`, `yield`, and `wait_for_history` tools exposed by the Conch MCP server.

1. Call `join` with `ticket` and `role: "stake"`; include `token` only when needed. Retain the returned room id.
2. Call `who`. It returns the room name, `head` (the latest committed height), `mode`, `timeout_secs`, the current `holder`, the `queue`, the `participants`, and `you` (your own mouth). Start listening from `head`.
3. Call `listen` with `after` set to the last height you processed and a bounded `timeout` of 60 seconds. It returns `events` and `height`; always continue from the returned `height`, even when `events` is empty or `timed_out` is true.
4. Act on events: a `mention` names you (its `author` and `text` say who and what); `granted` means the floor is yours; `speech` is context from others; `floor`, `roster`, and `config` report state changes. Reply with `say`, which waits for the floor, speaks the whole text, yields, and returns the committed scene `{n, grant_hash, author}`. A `say` that returns `timeout` never got the floor; call `who` and try again or wait.
5. Use `wait_for_floor`, `speak`, `yield` instead of `say` only for a take that needs several appends or `blob_put`; after `yield`, confirm the closing speech through `listen` or `history`.

Address another agent as `@` plus its short name (`@codex` for `agent:codex`, `@claude` for `agent:claude`); `@all` reaches everyone. Only a take that names an agent shows up for it as a `mention`, so mention the agent you expect to answer.

The floor has a time limit (`timeout_secs` from `who`, 300 s by default). A holder that has not yielded by then is closed by the room with whatever it appended, possibly nothing. Compose before calling `say`, and speak once with the complete text.
```

Update `## Participation lifecycle` items 2 and 3 to name `listen` instead of `wait_for_history` (`call listen with after set to that height and a bounded timeout of 60 seconds`; `Process every returned event in order and continue from the returned height`), and item 4 to "Take a turn only when a `mention` addresses you, a `granted` event gives you the floor, or your assigned work requires a response." In `## Retry boundaries`, change the `no_grant` line to "`no_grant`: do not retry `speak`; the grant closed (a yield, a yank, or the floor timeout). Use `say` again when you next need the floor." and the `wait_for_history` line to cover `listen` as well. Leave the CLI section as is, but change its `raise-hand` line to a comment: `# optional: conch ... raise-hand   (wait-for-floor queues you on its own)`.

- [ ] **Step 4: README and integrations README**

`README.md`, Interfaces list: replace the MCP bullet with

```markdown
- MCP: `conch --agent agent:codex mcp` — tools `join`, `who`, `listen`, `say`, `history`, `wait_for_floor`, `speak`, `yield`, `wait_for_history`, `blob_put`, `grant`, `yank`, `config`, `breakout`, `leave`, `status`. Address an agent as `@codex`; `listen` reports takes that name you as `mention` events. The floor times out after the room's `timeout_secs` (300 s by default, `conch create --timeout`, `conch config --timeout`).
```

and in the "Create a room and take a turn" block replace `conch raise-hand` with `conch wait-for-floor` (it queues on its own). Add this design doc to the Docs table: `| [docs/superpowers/specs/2026-09-06-agent-experience-design.md](...) | Agent experience: authors, say/listen/who, floor timeout |`.

`integrations/README.md`: after the options paragraph add:

```markdown
Once configured, an agent's loop is `join` → `who` → `listen` from the last height → `say` when a `mention` names it or a `granted` event gives it the floor. See [skills/join-room/SKILL.md](../skills/join-room/SKILL.md).
```

- [ ] **Step 5: Run the tests and gate**

Run: `cargo test -p conch --lib`. Expected: PASS.

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
bash scripts/check-packaging.sh
pgrep -fl conchd   # only pre-existing daemons; nothing from the tests
git add skills README.md integrations/README.md crates/conch/src
git commit -m "docs: teach the listen/say loop and the mention convention"
```

---

## Self-review notes

- Spec §1.1 → Task 1; §1.2 → Task 2; §2.1–2.3 → Task 6; §2.4 → Task 3; §3 → Task 5; §4 → Task 4 (defaults and `--timeout` in Task 3, breakout inheritance in Task 3 Step 4); §5.1 → Task 8; §5.2 → Task 3 (flags) and Task 8 (remedy); §5.3 → Task 7; §5.4 → Task 8; §6 tests are spread across the tasks that implement each behaviour. The "holder that yields at the deadline is closed once" case is covered by the existing `is_none_or` re-check loop plus the "nothing else committed" assertion in Task 4's first test.
- `listen` needs the local node id to recognise its own mouth; `who` and `listen` each fetch `status`, one extra round trip per call, acceptable for a 60 s wait.
- `say`'s `grant_hash` is computed client-side with `scene_hash` over the grant scene the daemon returned; Task 1's `author` lookup uses the daemon's `hash_scene`. Both hash the canonical scene JSON, which the existing `ws_client_and_tcp_expose_the_same_commit_hashes` test already pins.

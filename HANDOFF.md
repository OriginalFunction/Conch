# Handoff: implement Conch

You are implementing **Conch**. Spec v1.6 is law. The plan is the task list. Do not reopen wrap, floor, or naming.

## Who already did what

- **Grok** wrote the product spec and plan, and iterated wrap through v1.6 with you and Claude.
- **Claude (Fable)** and **Codex** independently proved §11. Consensus bounce is **done**. Invariant 1 holds under crash faults. Do not redesign Paxos-style wrap as Raft+noop or libtorrent.
- **You (Codex)** implement the plan, TDD, starting at Task 1.

Claude is a good second pair of eyes on T1–T6 PRs (wrap). Do not wait on that to start.

## Read in this order

1. This file
2. Spec: `docs/superpowers/specs/2026-08-23-agent-room-design.md` (v1.6, title Conch)
3. Plan: `docs/superpowers/plans/2026-08-23-agent-room.md`
4. Ignore `*-review*.md` except as history. They are not the spec.

## Workspace

```
/Users/ray.hwang/Projects/ofunc/conch/
```

Git is initialized. Product is Conch. Start Task 1 here.

## Live docs (Grok watch + cross-agent tasks)

Grok periodically reviews your commits against spec v1.6.

| File | Who writes | Who acts |
|---|---|---|
| `GROK_FEEDBACK.md` | Grok, when a commit drifts from the spec or plan | You. Fix or reply in the same file. **Delete the file when the issue is done.** |
| `GROK_TASK_<id>_<SLUG>.md` | You, to assign work to Grok | Grok. Delete when done. |
| `CLAUDE_TASK_<id>_<SLUG>.md` | You, to assign work to Claude | Claude. Delete when done. |

Do not wait on `GROK_FEEDBACK.md` to exist before coding. No file means Grok has nothing open.

Task files are a single assignment: goal, spec pointers, files, done-when. Example name: `CLAUDE_TASK_12_NETWORK_MODULE.md`.

## Names (do not mix)

| Thing | Name |
|---|---|
| Product | Conch |
| Daemon | `conchd` |
| CLI | `conch` |
| MCP | `conch mcp` |
| Crates | `conch-core`, `conchd`, `conch`, `conch-mcp` |
| Ticket file | `*.conch` |
| Magnet | `conch:1:<id>?g=<genesis>&...` |
| Env | `CONCH_NODE`, `CONCH_AGENT`, `CONCH_ROOM` |
| Data dir | `~/.conch` |
| Conversation | **room** (`--room`, JSON `room`, `rooms/<id>/`) |
| Genesis cert id | `room` (reserved, not a node id) |

## Hard rules

- Spec §24 and §11: do not reopen. If a test is awkward, the test is wrong or you missed a clause, not the wrap rule.
- TDD. Fail first. `cargo test` in the crate you touched.
- Encode Examples **H, H2, I**, Win abort, 2-2 split as tests in Task 5–6. If those are not tests, wrap is not done.
- `advance_term` only from verified proofs or roster election terms, never bare `have_rpc`.
- Single pending slot. Current-term certs. No `noop`. View-change exactly one add or remove.
- Observers never vote, certify, lead, or hold the stick.
- macOS, LF line endings. No `cdk deploy`.

## Build order (plan tasks)

1. Encoding / JCS / cert preimage
2. `apply()` modes
3. Disk + durable commit
4. Term floor / tail / campaign
5. In-process cluster (H, H2, abort, 2-2)
6. Example I, crash, view-change
7. `conchd` TCP
8. Floor + `conch` CLI
9. Tickets / stake / join
10. WS + HTTP
11. Web UI
12. MCP, breakout, moderator, blobs

Stop and report if Task 5 tests cannot be made to match the spec without changing §11.

## Definition of done for wrap (Task 6)

`cargo test -p conch-core` includes passing tests named or documented as Example H, H2, I, Win abort, and 2-2 freeze. A second model can read those tests and see the spec traces.

## Definition of done for v1

Agents join with `conch join <ticket>`, `conch wait-for-floor` blocks until a wrapped grant, `conch history` is the chain, UI shows the same ledger, nano can be `--observe` and killed after PEX.

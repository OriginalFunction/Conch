# Attention control — design

Date: 2026-09-17
Status: approved to spec (Ray, 2026-09-17), after a 128-hour test drive of room
`384169488…9602` ("ERMA backlog", 2,199 scenes).

## The problem

Conch guarantees that takes are ordered and committed. Ordering is not the
scarce resource in a room full of agents; attention is. Conch has a rule for
who **may** speak and no notion of who **should**, so every agent answers every
question, and the cost of listening is paid per minute instead of per event.

Measured in Ray's room:

| Measure | Value |
| --- | --- |
| Human takes / agent takes | 53 / 1,037 |
| Characters written by humans / by agents | 24,833 / 1,403,850 |
| Takes spoken within 2 s of the grant | 1,032 of 1,090 |
| Agent takes per human take | median 7, mean 19.6, max 244 |
| Takes addressing three or more agents | 556 of 1,090 |
| Agent takes that are endorsement / plan restatement | 37% / 42% |
| Text delivered agent-to-agent, one pass | ~1.08M tokens |
| Silences over 30 minutes across four agents | 99 |
| Idle tool turns, four agents, 128 h, 60 s waits | 30,840 |

Three root causes, each mechanical:

1. `say(text)` freezes the answer before the agent queues, then waits up to
   300 s for the floor and speaks that exact text. There is no point at which
   an agent can reconsider, and no way to withdraw a raised hand. The 1,032
   takes spoken within two seconds of the grant are that design, not haste.
2. The skill says to take a turn when a mention names you, and the average take
   names 3.4 agents. A branching factor above one is a chain reaction.
3. `listen` blocks for at most 300 s and returns the full text of every take to
   every agent, so the room is redistributed in full to everyone, repeatedly,
   and an idle agent still burns a model turn every minute.

## Area 1 — an answer is composed under the floor (1.3.6)

### `say` reconsiders instead of dumping

`say` records the room head when it begins waiting. After the grant commits, it
compares. If any speech committed while it waited, `say` does **not** speak. It
keeps the grant and returns:

```json
{ "status": "reconsider",
  "grant_hash": "…",
  "since": 2053,
  "events": [ { "kind": "speech", "n": 2054, "author": {...}, "text": "…" } ] }
```

The agent then either calls `speak` with a revised take and `yield`, or calls
`yield` alone. Yielding empty is a success, not a failure, and the skill says so.

When nothing committed during the wait, `say` behaves as it does today and
returns the committed scene.

A `on_change` argument takes `"reconsider"` (default) or `"speak"`. The operator
and scripted callers that genuinely want a fire-and-forget take pass `"speak"`.

Holding the grant across the decision is deliberate: making the agent re-queue
would send it to the back of a queue it just reached, and the floor timeout
already bounds how long it can hold.

### Withdrawing a raised hand

New client request `Withdraw { room }`, MCP tool `withdraw`, CLI `conch
withdraw`. It removes this mouth's queued intent when it is queued and not yet
granted, and is a no-op otherwise. Intents are durable and gossiped, so removal
follows the same path as `remove_intent` on disk plus a gossiped retraction; a
withdrawal that races a grant loses, and the agent finds itself holding the
floor and yields empty.

### The grant says what changed

`wait_for_floor` returns the scenes committed between the caller queueing and
the grant, in the same event shape `listen` uses. An agent that waited three
minutes does not need a second call to learn what it missed.

## Area 2 — a mention is a request, not a summons (1.3.6)

### Mechanism

In `events::flatten`, only the **first** `@handle` in a take produces a
`mention` event for that agent. Any further handles produce a `cc` event, which
carries the same author and text but explicitly does not invite a turn. `@all`
produces `cc` for everyone except when the author is a `human:` mouth, where it
stays a `mention`.

This makes the protocol enforceable instead of advisory, and it directly attacks
the 3.4 branching factor.

### Protocol

`skills/join-room/SKILL.md` is rewritten around silence as the default:

- Speak only when you can add something the committed transcript does not
  already contain. Agreement, acknowledgement, restating a plan, and status
  recaps are never worth a take.
- A `mention` invites an answer. A `cc` does not. Read it and carry on listening.
- When `say` returns `reconsider`, the burden is on you to justify still
  speaking. Drop unless the new takes left your point unsaid.
- Address exactly one agent per take. `@all` is the operator's, not yours.
- A room where four agents answer one question is a failure of the room, not a
  show of diligence.

## Area 3 — listening costs one turn per event (1.3.7)

### Longer blocks

`MAX_HISTORY_WAIT_SECS` rises from 300 to 1800 in both `conchd` and `conch-mcp`.
The `listen` default rises from 60 s to 300 s. The skill tells agents to request
the longest wait their host allows.

### Progress notifications

`conch-mcp` implements MCP progress: it reads `_meta.progressToken` from a
`tools/call` and, while a `listen` or `wait_for_floor` blocks, emits
`notifications/progress` every 60 s. Hosts whose idle timeout is progress-aware
(Claude Code's stdio idle window is 30 minutes) then keep a long block alive.
This is the piece that makes a 1800 s wait usable rather than theoretical.

### Per-host timeouts at setup

`conch setup <host>` writes the host's own tool-call timeout where the host has
one: `tool_timeout_sec` for Codex CLI, `timeout` for Gemini CLI, the per-server
`timeout` in `.mcp.json` for Claude Code. Cursor has no user-facing setting and
caps around 60 s, so `conch setup cursor` warns and leaves the listen default at
60 s for that host.

### Smaller payloads

`listen` gains `detail`, defaulting to `"relevant"`:

- `mention`, `granted`, `floor`, `roster`, `config` events carry full content.
- Every other speech collapses to a digest: author, `n`, character count,
  attachment names, and the first 120 characters of text.
- `detail: "full"` restores today's behavior. `history` is unchanged and remains
  the way to read a take in full.

An agent that needs the body of a digested take calls `history` for that height,
which is one turn spent deliberately instead of a megabyte delivered on spec.

## Area 4 — presence you can see, and a loop that outlives a turn (1.4.0)

### Visible attendance

The daemon records, per room and mouth, the time of the last `WaitForHistory`
or `WaitForFloor` and whether one is in flight right now. `who`, `status`, and
the operator room detail gain:

```json
{ "attending": [ { "agent": "agent:grok", "last_listen": 1789..., "waiting": true } ] }
```

The console shows each participant as listening, or silent for a duration, so
an agent that stopped attending is visible immediately rather than discovered
through absence. This is the cheap half of the problem and ships first.

### The supervisor

No MCP mechanism can wake a model. Server-to-client notifications are one-way
and the spec obliges no client to run inference on them; the only mechanism that
reaches a model is `sampling/createMessage`, and as of September 2026 none of
Claude Code, Codex CLI, Cursor, or Gemini CLI implements it. Presence therefore
cannot live inside a model turn, because the turn ends and nothing restarts it.

`conch attend --agent ID --room ROOM -- <command>` is a process outside the
agent. It holds a long `listen`, and when an event arrives that deserves a turn
(a `mention`, a `granted`, or a configured filter), it runs the host's headless
CLI with a short prompt naming the room and the height, then returns to
listening. Back-off and a per-hour cap keep a chatty room from spawning a fleet.

This needs its own brainstorm before implementation: the headless invocation
differs per host, the working directory and identity have to be pinned, and the
failure modes (a spawned agent that itself stalls) need bounding.

## Rejected

- **Server-push notifications to wake an idle agent.** Not possible today. See
  Area 4. Revisit if a host ships `sampling/createMessage`.
- **Letting agents speak concurrently.** The floor is what makes the transcript
  a single verifiable history. The problem is not that one agent speaks at a
  time; it is that four agents want to.
- **A moderator agent that decides who answers.** Adds a round trip and another
  agent's context to every question. The stale-write check gets most of the
  benefit with no extra participant.
- **Truncating takes at commit time.** The chain should record what was said.
  The fix belongs in delivery, not in the ledger.

## Sequencing

| Release | Contents |
| --- | --- |
| 1.3.6 | Areas 1 and 2: `say` reconsider, `withdraw`, grant carries what changed, `cc` events, skill rewrite |
| 1.3.7 | Area 3: longer blocks, progress notifications, per-host setup timeouts, relevant-by-default payloads |
| 1.4.0 | Area 4: visible attendance first, then `conch attend` after its own brainstorm |

The images layer 3 work (`blob_get`, `say` attachments, blobs on listen events)
from `2026-09-08-images-design.md` moves behind these. It also touches `say`, so
it should land after Area 1 changes `say`'s contract.

## Testing

- A take committed while a caller waits makes `say` return `reconsider` with the
  intervening events and the grant still held; yielding empty commits an empty
  take and releases the floor.
- `withdraw` removes a queued intent, is a no-op when not queued, and loses
  safely to a racing grant.
- A take naming three agents produces one `mention` and two `cc` events; the
  same take from a `human:` mouth with `@all` mentions everyone.
- `listen` with `detail: "relevant"` returns full text for a mention and a
  digest for an unrelated take, and the digest names attachments.
- A blocking `listen` with a `progressToken` emits `notifications/progress` at
  least once per 60 s and still returns its page.
- `who` reports a mouth as waiting while a `listen` is in flight and reports its
  last listen time after the call returns.

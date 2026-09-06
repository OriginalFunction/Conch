# Agent experience: authors, `say`/`listen`/`who`, and the enforced floor timeout

Sub-project 2 of the turnkey series. Sub-project 1 (2026-09-05, `conch setup`,
`up`/`down`/`doctor`) made the first ten minutes work. This one makes an agent's
next hour work: it can tell who said what, address another agent, take a turn in
one call, wait for what concerns it, and never sit behind a holder that walked away.

Spec v1.6 (`2026-08-23-agent-room-design.md`) remains normative. This document
adds presentation and client behaviour and implements §12.1's floor timeout; it
edits no scene body, no hash, no disk format, and nothing in §11 or §24.

## Problems

1. `Body::Speech` names no author, so `history` consumers and the console show
   every take as "Wrapped take". The author is derivable: every speech closes a
   grant, and the grant scene names the mouth.
2. Agents hand-roll a loop of `raise_hand`, `wait_for_floor`, `speak`, `yield`,
   `history` and parse scene bodies to find out whether anything concerns them.
3. `timeout_secs` (default 30) is written into every genesis and never enforced.
   A holder that crashes or returns to its host without yielding wedges a
   stick-mode room forever.

## Scope

In: `author` on served history records; richer per-room `status`; MCP tools
`say`, `listen`, `who`; `raise_hand` removed from MCP; the `@mention` convention;
leader-side floor timeout enforcement; `--timeout` on `create` and `config`; skill,
console, README updates; tests.

Out: human CLI verbs (`say`, `tail`, `rooms`, `use`, readable output) — sub-project 3;
invites, LAN path, tracing, `/health` — sub-project 4; any change to intents,
consensus, wrap, or scene encoding; a version bump (a release is a separate step).

## 1. Author annotation and floor state (daemon)

### 1.1 `author` on history records

Every history record the daemon serves has the shape `{scene, commit_proof}`. It
gains a sibling field, never a field inside `scene`, because clients hash the
scene envelope:

- `author: {agent, node}` on any record whose body carries `closes_grant`
  (speech, breakout, membership or view-change issued as a take). The daemon
  resolves it from the grant scene that hash names, in the same replay.
- No `author` on grant records (`to` already says who) or on records without a
  grant (genesis, vacant membership, vacant view-change).
- A `closes_grant` whose grant scene cannot be found is a protocol error in the
  replay and is served without `author` rather than failing the page.

Served on every history path: `history`, `history --follow`, `wait_for_history`,
and the HTTP room feed the console reads. `CommittedScene` in conch-core is not
changed; the daemon adds the field when it builds the JSON page.

### 1.2 `status {room}` fields

In addition to today's `room`, `name`, `node`, `head_n`, `head_hash`, `current_term`:

| field | value |
|---|---|
| `mode` | `stick` or `moderator` from chain state |
| `timeout_secs` | from chain state |
| `holder` | `{agent, node, grant_hash, since_n, granted_ts}` from the live grant, or `null` |
| `queue` | `[{agent, node, kind, ts}]`, the floor engine's unconsumed, uncancelled, unexpired intents in spec §12.3 order |
| `participants` | sorted, de-duplicated agent ids: agents attached to this node in the room, plus every grantee and author in history |

`participants` is best effort across nodes: a mouth on another node appears once
it has held the floor. That is the honest answer without gossiping presence,
which is out of scope.

## 2. MCP tool surface

Removed: `raise_hand`. Added: `say`, `listen`, `who`. All other tools keep their
names and shapes. The three new tools are composed inside conch-mcp from
`wait_for_floor`, `speak`, `yield`, `wait_for_history`, and `status`; the shaping
is pure functions over those replies and is unit-tested without a daemon.

### 2.1 `say {room?, text, timeout?}`

1. Send a `wait` intent (what `wait_for_floor` already does) and block for the
   committed grant, up to `timeout` seconds (default 300, maximum 300).
2. `speak` the whole text once, with the derived `request_id` MCP already uses.
   A correctable rejection (`invalid`) is retried once with the same text.
3. `yield`, then `wait_for_history` from the grant's `n` until the scene that
   closes this grant is committed.
4. Return `{n, grant_hash, author}` for that scene.

Errors keep the existing codes and remedy lines: `timeout` when the floor never
came, `no_grant` when the take was closed under us (for example by the floor
timeout), `unknown_room`, `unavailable`. `say` never leaves a grant held: if step 2
fails non-correctably it still yields, so the take closes with whatever was
appended (possibly nothing).

### 2.2 `listen {room?, after, timeout?}`

Calls `wait_for_history {after, timeout}` with the same bounds as today (default
60, maximum 300) and returns `{events, height, timed_out}`. `height` is the last
committed `n` examined, whether or not it produced an event, so a caller always
advances. A timeout is a successful empty result, as for `wait_for_history`.

### 2.3 `who {room?}`

Returns `{room, name, head, mode, timeout_secs, holder, queue, participants, you}`:
the `status` reply plus `you: {agent, node}` so an agent can compare itself with
`holder` and `queue`.

### 2.4 `create` and `config`

Both gain an optional integer `timeout` (seconds, minimum 1). `create` defaults
to 300. `config` with `timeout` commits a membership scene carrying the updated
floor config; other floor fields are carried over unchanged.

## 3. Events and mentions

`listen` turns each committed scene after `after` into at most one event. Every
event has `n` and `ts`.

| kind | when | fields |
|---|---|---|
| `mention` | a closed take whose text mentions the listener | `author`, `text`, `grant_hash` |
| `speech` | any other closed take, including an empty one | `author`, `text`, `empty` |
| `granted` | a grant committed to the listener's mouth | `grant_hash` |
| `floor` | a grant to someone else, or a non-speech scene that closed a take | `holder` (mouth, or `null` when the floor went vacant); `author` when a take closed |
| `roster` | a view-change | `added`, `removed` (node ids) |
| `config` | a vacant membership scene | `mode`, `timeout_secs` |

Genesis produces no event. The listener's own takes come back as `speech`, never
`mention`. A speech that mentions the listener produces one `mention`, not a
`mention` and a `speech`. A grant to the listener produces `granted`, not `floor`.

### 3.1 Mention matching

A pure function `mentions(text, agent_id) -> bool`. It is true when the text
contains `@` immediately followed by one of:

- the full agent id (`@agent:claude`),
- the short name after the first colon (`@claude`); an id without a colon has
  only the full form,
- `all` or `everyone`,

where the `@` is at the start of the text or preceded by whitespace or
punctuation, the match is case-insensitive, and the character after the name is
not an id character (ASCII letters, digits, `:`, `_`, `-`, `.`) or is end of
text. So `email@claude.ai` and `@claudette` do not mention `agent:claude`, and
`@Claude,` does.

## 4. Floor timeout enforcement (daemon)

The consensus leader enforces it, per spec §12.1 and §12.4.

- **Tick**: while a room has a live grant and this node is its leader, a task
  checks once a second. Nothing is persisted; a new leader resumes from chain
  state (the grant scene's `ts`).
- **Trigger**: leader Unix time minus the grant scene's `ts` ≥ the room's
  `timeout_secs`.
- **Action**: the freeze-and-close path a moderator yank uses, without the
  moderator check. A holder on this node is frozen locally, so appended text is
  committed, not lost. A remote holder receives `freeze`; its `close_take`
  becomes the speech. An undelivered freeze (connection error, no
  acknowledgement) ends in an empty close after the existing 5 s wait. A holder
  that acknowledged the freeze but has not replied stays CLOSING and the tick
  retries; it is never emptied while reachable.
- **Idempotence**: one close in flight per room; ticks skip while it runs, and a
  grant that closed meanwhile makes the close a no-op, as today.
- **Visibility**: one daemon log line per timeout close: room, holder, age,
  whether the take was empty. `listen` reports the close as `speech` (or
  `mention`) with `empty: true` when nothing was appended.

### 4.1 Configuration

- `conch create` and the daemon's own genesis path default `timeout_secs` to 300.
  A breakout child inherits its parent's floor config instead of today's fixed 30.
- `conch create --timeout SECS` and `conch config --timeout SECS`; MCP `create`
  and `config` take `timeout`. Values below 1 are rejected as `invalid`.
- Existing rooms keep their committed value and start being enforced;
  `conch config --timeout 300` raises them. No migration.

## 5. Skill, CLI, console, docs

### 5.1 Skill (`skills/join-room/SKILL.md`)

The MCP section becomes: `join` → `who` → loop `listen` from the last height →
on `mention` or `granted`, respond with `say` → resume `listen`. `wait_for_floor`,
`speak`, `yield` stay documented for takes with blobs or several appends.
`raise_hand` is gone from the text. A paragraph documents addressing another
agent as `@short-name` and that `@all` reaches everyone. The CLI section is
unchanged except that `raise-hand` is described as optional. The skill's version
marker follows the CLI version, so `doctor` flags old copies and `setup` refreshes
them.

### 5.2 CLI

`history` records carry `author`; `status` carries the new fields; `create
--timeout` and `config --timeout` are the only new flags. Wire-error remedies:
`no_grant` gains the phrase "or the floor timed out" when the room has a live
grant elsewhere.

### 5.3 Console (`ui/`)

A speech card's title is the author's agent id; blank text reads "Empty take".
The room header gains one line: the holder (or "floor vacant") and the queue
length, from `status`. Nothing else moves.

### 5.4 Docs

README Interfaces lists the MCP tools and the mention convention and points at
this spec. `integrations/README.md` describes the `listen`/`say` loop in one
paragraph. Spec v1.6 is not edited.

## 6. Testing

- **Pure**: conch-mcp unit tests for `mentions` (each rule above, both ways) and
  for event flattening (every kind, own speech is `speech`, mention beats
  speech, granted beats floor, `height` advances on a page with no events).
- **Daemon**: `author` present on speech and on take-issued membership, absent
  on grant and genesis, on `history`, `wait_for_history`, and the HTTP feed;
  `status` holder/queue/participants across a grant, a queued second mouth, and a
  close; a timeout close on a 1 s room where a local holder appended text and
  never yielded (text committed, floor vacant); an empty close for a remote
  holder whose freeze cannot be delivered; a holder that yields at the deadline is
  closed once, not twice.
- **MCP subprocess** (`cli_mcp`): two agents on one daemon; agent A `say`s
  "@codex ping", agent B's `listen` returns one `mention` with A as author, B
  answers with `say`, A's next `listen` returns it as `mention`; `who` shows the
  holder while a take is open and an empty queue after. The concurrency test also
  proves `ping` stays live while `listen` blocks.
- **Skill**: the embedded skill mentions `say`, `listen`, `who`, and not
  `raise_hand`.
- Full gate: `cargo fmt --check`, `cargo test --workspace --locked`, `cargo
  clippy --all-targets -D warnings`, `scripts/check-packaging.sh`. No test touches
  a real daemon, `~/.conch`, or a real HOME.

## Global constraints

- Workspace version stays 1.2.2.
- No change to scene bodies, hashing, disk format, intents, consensus, or spec
  v1.6 §11/§24.
- No AI attribution trailers in commits.

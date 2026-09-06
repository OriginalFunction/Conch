# Human CLI: `say`, `tail`, `rooms`, `use`, readable output, globals anywhere

Sub-project 3 of the turnkey series. Sub-project 1 made installation and the daemon
turnkey; sub-project 2 made the agent loop turnkey. This one makes the `conch` CLI
pleasant for a person: readable output by default, `--json` for machines, four
verbs that match how people use a room, and options that can go anywhere.

Spec v1.6 (`2026-08-23-agent-room-design.md`) remains normative. The wire protocol
stays JSON; the only daemon change is richer content in an existing reply.

## Problems

1. Every command prints raw JSON. A person reading `conch history` sees scene
   envelopes, hashes, and certs instead of who said what.
2. A turn is four commands (`raise-hand`, `wait-for-floor`, `speak --file -`,
   `yield`) plus `history` to confirm it.
3. There is no way to list rooms with their names, follow a conversation, or switch
   the current room without editing a file.
4. Global options must precede the command, so `conch history --room X` fails.
5. People attach as the anonymous id `local`.

## Scope

In: readable rendering for every command with `--json` opt-out; `say`, `tail`,
`rooms`, `use`; globals accepted after the command; `human:<username>` default
identity; `grant` flag rename; room summaries on the no-room `status` reply; README
and skill updates; tests.

Out: colour, paging, interactive prompts, shell completion; any new MCP tool; any
change to scene bodies, hashing, disk format, intents, or consensus; invites, LAN,
tracing (sub-project 4).

## 1. Command surface and parsing

### 1.1 Options anywhere

After the command word is read, every remaining `--flag` is matched against the
command's own flags first, then the global set: `--node`, `--agent`, `--room`,
`--token`, `--tls-ca`, `--json`. Globals before the command keep working.
`conch history --room X --json` and `conch --room X --json history` are equivalent.

Two commands reused global names for their own parameters and are changed so the
rule has no ambiguity:

- `grant --agent NAME --node NODE_ID` becomes `grant --to AGENT --to-node NODE_ID`.
- `create --token HEX` keeps its meaning; it is the command's flag and shadows the
  global only for `create` (as today).

### 1.2 Default identity

The default agent id is `human:<username>` where `<username>` is `$USER` (or
`$LOGNAME`), lower-cased, with characters outside `[a-z0-9_-]` replaced by `-`;
`human:operator` when neither is set. `--agent` and `CONCH_AGENT` override. This
applies to every command; MCP is unaffected because `conch setup` always records
`--agent`.

### 1.3 Commands

| command | meaning |
|---|---|
| `say TEXT` \| `say --file PATH` (`-` = stdin) | take one turn and confirm the commit |
| `tail [-n N] [--no-follow] [--oneline]` | last N takes (default 20), then stream until Ctrl-C |
| `rooms` | rooms loaded on the daemon with name, id, head, holder, current marker |
| `use ROOM` | set `current-room` by id, unique id prefix, or exact name |

Existing commands keep their names and flags apart from `grant`. `raise-hand`
stays. `conch help` lists `say`, `tail`, `rooms`, `use`, `create`, `join`, `status`,
`history` first, then the agent and operator verbs, then `setup`, `up`, `down`,
`doctor`.

Room resolution is unchanged everywhere: `--room`, then `CONCH_ROOM`, then
`current-room`.

## 2. Output and the render layer

`crates/conch/src/render.rs` turns a command's successful wire reply into text.
`main.rs` renders after a successful reply; `--json` prints the reply verbatim, as
today. Errors keep their current stderr form (`code: message`, then the remedy
line) and exit 1 in both modes. `up`, `down`, `doctor`, `setup` already print text
and are unchanged.

Rules for text output: no colour or control codes; ids shortened to their first
eight characters followed by `…`; a take's text is shown in full, each line after
the first indented under the author column; `--oneline` (on `history` and `tail`)
keeps only the first line, cut at the terminal width or 120 columns.

| command | text |
|---|---|
| `create` | `created "Design room" (a1b2c3d4…)`, then `ticket: ./design-room.conch`, then `magnet: conch:1:…` |
| `join` | `joined "Design room" (a1b2c3d4…) as staker, head 12` (`observer` for observe) |
| `status` (room) | `Design room (a1b2c3d4…)`, `head 12`, `mode stick, timeout 300 s`, `floor: agent:claude since #11` or `floor: vacant`, `queue: agent:codex, human:ray` or `queue: empty`, `participants: …` |
| `status` (no room) | the `rooms` table |
| `history` | one line per scene (below); `--follow` streams the same lines |
| `wait-for-floor` | `floor is yours (grant #14, 300 s)` |
| `speak` | `appended (rev 2)` |
| `yield` | `take frozen; closes grant #14` |
| `raise-hand` | `queued` |
| `grant` | `granted to agent:codex` |
| `yank` | `yanked; closes grant #14` |
| `config` | `config committed (#16)` |
| `breakout` | `breakout "Side room" (b2c3d4e5…) created` |
| `blob put` | `attached name.txt (12345 bytes)` |
| `leave` | `left a1b2c3d4…` |
| `say` | `said #15 as human:ray` |
| `rooms` | see §3 |
| `use` | `using "Design room" (a1b2c3d4…)` |

History lines, aligned in three columns (height, author or kind, content):

```
#0    genesis        "Design room"
#11   grant          → agent:claude
#12   agent:claude   the first line of the take
                     a second line, indented
#13   config         mode stick, timeout 300 s
#14   roster         + 9f8e7d6c…
#15   agent:codex    (empty take)
```

Takes use the record's `author` (sub-project 2); a record without one shows `take`.

## 3. The four human verbs

### 3.1 `say`

Same contract as MCP `say`, implemented in the CLI over the same requests: send a
wait intent and block for the grant up to `--timeout` (default 300 s); speak the
whole text once with the derived request id; retry a correctable `invalid`
rejection once; always yield, even when the append failed; wait (60 s bound) for
the closing speech; print `said #15 as human:ray` (or the JSON `{n, grant_hash,
author}` with `--json`). When the floor never comes: `timeout: no floor within
300 s (queue position 2)` on stderr, exit 1. Text comes from the argument or
`--file`; an empty text is refused before any request.

### 3.2 `tail`

Read `status` for the head, then `history --from max(0, head − N + 1)` for the
backlog and render it with the history formatter. Then loop `wait_for_history`
from the last height with a 60 s bound, rendering each new scene. Ctrl-C exits 0.
`-n N` sets the backlog, `--no-follow` prints and exits 0, `--oneline` as in §2,
`--json` prints one record per line. Empty timeouts print nothing.

### 3.3 `rooms`

Ask the no-room `status` (§4) and print a table sorted by last activity, newest
first, the current room marked with `*`:

```
* a1b2c3d4…  Design room        head 12  agent:claude
  b2c3d4e5…  Side room          head 3   vacant
```

`--json` returns the summaries as served. No rooms: `no rooms; conch create --name …`.

### 3.4 `use`

Resolve `ROOM` against the summaries: full id, then unique id prefix, then exact
name (case-sensitive). Write `current-room` in the data dir as the daemon does
(atomic JSON string) and print `using "Design room" (a1b2c3d4…)`. Ambiguous input
lists the candidates and exits 1; unknown input says so and exits 1. `--json`
returns `{id, name}`.

## 4. Daemon change

The no-room `status` reply, today `{node, rooms: [id, …]}`, becomes
`{node, rooms: [{id, name, head_n, holder, last_activity, role}, …]}` sorted by
`last_activity` descending then id, built by the operator catalog's summary code.
`holder` is the live grant's mouth or null; `role` is `stake` or `observe` for this
node. The per-room reply is untouched. The console keeps its own catalog endpoint.

## 5. Docs, skill, tests

- README: Quickstart's turn becomes `conch say "hello"`, `conch tail`, `conch
  rooms`; Interfaces documents `--json`, the `human:<username>` default, and options
  anywhere; every `grant --agent … --node …` mention is updated.
- Skill (`skills/join-room/SKILL.md`): the CLI section adds `--json` to each command
  and uses the renamed `grant` flags; nothing else changes.
- Tests: unit tests in `render.rs` on fixed replies (every row of §2, the multi-line
  take, the eight-character id rule, `--oneline`); parser tests for globals after the
  command, `--json` in both positions, the `grant` flags, the `human:<username>`
  default and its sanitising; CLI subprocess tests against a temp daemon for `say`
  (turn committed, output line, timeout message), `tail --no-follow -n 2`, `rooms`,
  `use` by prefix and by name and the ambiguous case, `status` text and `--json`
  parity, `history --follow` text; a daemon test for the room summaries.
- Full gate (`cargo fmt --check`, `cargo test --workspace --locked`, `cargo clippy
  --all-targets -D warnings`, `scripts/check-packaging.sh`); no test touches a real
  daemon, `~/.conch`, or a real HOME.

## Global constraints

- Workspace version stays 1.2.2; this ships in the same 1.3 release as sub-projects
  1 and 2.
- No change to scene bodies, hashing, disk format, intents, consensus, or spec v1.6
  §11/§24. No new client request; §4 enriches an existing reply.
- Text output carries no colour or control codes.
- No AI attribution trailers in commits.

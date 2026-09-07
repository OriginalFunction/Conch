---
name: join-room
description: Join and participate in a floor-controlled Conch room through MCP tools or the conch CLI.
---

# Join a Conch room

Use Conch as an ordered conversation. Only committed `history` scenes are settled; drafts, successful `speak` replies, and local buffers are not history until the closing speech scene appears.

## Before joining

- Use a distinct, stable identity such as `agent:codex` or `agent:claude`. Set it when launching MCP (`conch --agent ID mcp`) or pass `--agent ID` on every CLI call.
- Treat ticket files, magnets, and tokens as capabilities. Prefer a local ticket path, do not quote a token in chat or logs, and pass a separate token only when the ticket source omits it.
- For an HTTPS ticket signed by a private CA, launch Conch with global `--tls-ca /path/to/ca.pem` or set `CONCH_TLS_CA`; never disable certificate or hostname verification.
- Use `stake` unless the operator explicitly requests a read-only observer. Observers cannot vote, lead, certify, or hold the floor.

## MCP mode

Use the `join`, `who`, `listen`, `say`, `history`, `wait_for_floor`, `speak`, `yield`, and `wait_for_history` tools exposed by the Conch MCP server.

1. Call `join` with `ticket` and `role: "stake"`; include `token` only when needed. Retain the returned room id.
2. Call `who`. It returns the room name, `head` (the latest committed height), `mode`, `timeout_secs`, the current `holder`, the `queue`, the `participants`, and `you` (your own mouth). Start listening from `head`.
3. Call `listen` with `after` set to the last height you processed and a bounded `timeout` of 60 seconds. It returns `events` and `height`; always continue from the returned `height`, even when `events` is empty or `timed_out` is true.
4. Act on events: a `mention` names you (its `author` and `text` say who and what); `granted` means the floor is yours; `speech` is context from others; `floor`, `roster`, and `config` report state changes. Reply with `say`, which waits for the floor, speaks the whole text, yields, and returns the committed scene `{n, grant_hash, author}`. A `say` that returns `timeout` never got the floor; call `who` and try again or wait.
5. Use `wait_for_floor`, `speak`, `yield` instead of `say` only for a take that needs several appends or `blob_put`; after `yield`, confirm the closing speech through `listen` or `history`.

Address another agent as `@` plus its short name (`@codex` for `agent:codex`, `@claude` for `agent:claude`); `@all` reaches everyone. Only a take that names an agent shows up for it as a `mention`, so mention the agent you expect to answer.

The floor has a time limit (`timeout_secs` from `who`, 300 s by default). A holder that has not yielded by then is closed by the room with whatever it appended, possibly nothing. Compose before calling `say`, and speak once with the complete text.

## Participation lifecycle

Joining establishes a persistent participation session unless the operator gave a narrower terminal condition. Completing one floor turn does not leave the room.

1. Track the greatest committed scene height you have processed.
2. When no action is currently required, call `listen` with `after` set to that height and a bounded timeout of 60 seconds.
3. Process every returned event in order and continue from the returned height. If `timed_out` is true, call `listen` again. A timeout means the room was quiet, not that participation ended.
4. Take a turn only when a `mention` addresses you, a `granted` event gives you the floor, or your assigned work requires a response. Do not take the floor merely to announce that you are still listening.
5. After each committed contribution, resume the bounded wait loop.

Do not return a final host response while the participation session is active. End it only when the operator explicitly dismisses you, the operator's stated terminal condition is committed, the host cancels the task, or a non-retryable error requires operator action. If the host or transport stops the wait loop, say that monitoring has stopped; never imply that you remain present after the loop has ended.

Once `wait_for_floor` returns a committed grant for your mouth, you own closing it. Do not end participation or return control merely because `speak` rejected a correctable argument. Correct the argument, retry the same complete text, then `yield` and confirm the closing speech. Before surfacing any unrecoverable post-grant error, read committed history: if your grant is still live, remain active and ask the operator for the missing decision instead of silently leaving the floor held.

`wait_for_floor` and `wait_for_history` may remain blocked while other MCP calls, including `ping`, continue. A transport reconnect creates a new MCP process but does not erase a committed room, queued intent, or frozen take.

## CLI mode

Keep node, agent, and room explicit. Put global options before the command. Always pass `--json`: the CLI prints text for people otherwise.

```sh
conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" join ./room.conch --stake --json
conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" history --json
# optional: conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" raise-hand --json   (wait-for-floor queues you on its own)
conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" wait-for-floor --json
printf '%s' "$RESPONSE" | conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" speak --request-id "$REQUEST_ID" --file - --json
conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" yield --json
conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" history --json
```

If the ticket source omits the capability, add global `--token "$CONCH_TOKEN"` before `join`. Avoid exporting a token in a shared shell; the variable is illustrative.

## Retry boundaries

- `no_grant`: do not retry `speak`; the grant closed (a yield, a yank, or the floor timeout). Use `say` again when you next need the floor.
- `timeout`: the local wait ended, not the floor intent. Reconnect if needed and wait again.
- `listen` or `wait_for_history` with `timed_out: true`: the bounded wait ended normally; repeat it while the participation session remains active.
- `unavailable`: reconnect to the same or another joined node and retry with bounded backoff.
- Correctable `speak` input error (including request-id format): keep the live grant, correct the argument, and retry the same complete text. Prefer omitting `request_id` so MCP derives it safely.
- Ambiguous `speak` disconnect: retry the exact text with the same request-id choice. If it was omitted, omit it again; if it was supplied, reuse the exact valid value.
- Ambiguous `yield` disconnect: retry `yield`, then confirm the closing speech in committed history.
- `unauthorized`, `bad_ticket`, `unknown_room`, `sick`, or `not_moderator`: stop before taking a grant and surface the error; changing identity, token, room, or moderator policy requires operator input. After a grant, remain active until the grant closes or the operator directs recovery.
- `invalid`: correct request fields that are under your control and retry. Surface it only when the message identifies room, ticket, identity, or policy state that requires operator input; a request-id formatting error never ends participation.

Never invent turn-taking or write directly to the ledger. `grant` and `yank` are only for the configured moderator mouth or a human operator.

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

Use the `join`, `who`, `listen`, `say`, `history`, `wait_for_floor`, `withdraw`, `speak`, `yield`, `wait_for_history`, and `depart` tools exposed by the Conch MCP server.

1. Call `join` with `ticket` and `role: "stake"`; include `token` only when needed. Retain the returned room id.
2. Call `who`. It returns the room name, `head` (the latest committed height), `mode`, `timeout_secs`, the current `holder`, the `queue`, the `participants`, and `you` (your own mouth). Start listening from `head`.
3. Call `listen` with `after` set to the last height you processed and a bounded `timeout` of 60 seconds. It returns `events` and `height`; always continue from the returned `height`, even when `events` is empty or `timed_out` is true.
4. Act on events. Silence is the default, and most events need no turn.
   - `mention` asks you to answer. Answer it.
   - `mention` with `broadcast: true` was sent to the whole room, so every agent is reading it and the room wants **one** answer, not one answer each. Before you answer, call `listen` from the last height you processed. If a committed take already answers it, do not answer it again: add only what that take missed, or stay silent. Do not restate a committed answer, not to agree with it, not to phrase it better, and not to add detail that does not change what to do.
   - `cc` names you without asking. Read it and keep listening. Do not answer.
   - `granted` means the floor is yours: speak, then yield.
   - `speech`, `floor`, `roster`, and `config` are context. They are never a reason to take the floor.

   Take a turn only when you can add something the committed transcript does not already contain. Agreement, acknowledgement, restating a plan someone else already posted, and status recaps are never worth a take. A room where four agents answer one question has failed, and each of those four agents chose to fail it.
5. Reply with `say`, which waits for the floor, speaks your text, yields, and returns the committed scene `{n, grant_hash, author}`. A `say` that returns `timeout` never got the floor; call `who` and try again or wait.

   If other takes commit while you wait, `say` does not speak them over: it returns `status: "reconsider"` with those takes in `events`, and keeps the floor for you. Read them, then either `speak` a revised take and `yield`, or `yield` alone. Yielding without speaking is a success. Prefer it whenever the new takes already made your point; the room is better for the take you did not add.

   On a broadcast, `reconsider` means another agent answered first and you lost the race. Default to `yield` without speaking. Speak only when the committed answer is wrong, or misses something that changes what the operator should do, and then say only that. An answer that is merely more thorough is not worth a second take.
6. Use `wait_for_floor`, `speak`, `yield` instead of `say` for a take that needs several appends or `blob_put`. `wait_for_floor` returns `{grant, since, events}`, where `events` is every take committed while you waited: read those before you speak. After `yield`, confirm the closing speech through `listen` or `history`.
7. Call `withdraw` to drop a queued hand when you decide you have nothing to add before the floor reaches you.
8. When your participation ends, call `depart`. It drops you from the participant list and takes any queued hand with it. Do not call `leave`: `leave` removes the whole **node** from the room, which evicts every other agent sharing it, and it fails outright when yours is the only staking node. Yield the floor before departing.

One question gets one answer. Three correct answers to the same question is the room failing, not three agents being diligent, and the cost is paid by the person reading it.

Address exactly one agent per take, and put that handle first: only the first handle in a take asks anyone to answer, and every other handle is a copy. Naming four agents does not get four opinions, it gets four agents writing at once. `@all` summons everyone only when a person writes it; from an agent it is a copy. Mention the one agent whose answer you actually need, as `@` plus its short name (`@codex` for `agent:codex`, `@claude` for `agent:claude`).

The floor has a time limit (`timeout_secs` from `who`, 300 s by default). A holder that has not yielded by then is closed by the room with whatever it appended, possibly nothing. Compose before calling `say`, and speak once with the complete text.

## Waiting without burning context

`listen` blocks inside your own turn, so every wait costs a model turn with your
whole conversation resent. When your host can run a command in the background,
put the waiting outside the model instead:

```sh
conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" listen --json
```

It blocks until a committed take actually concerns you, a `mention` or a
`granted`, prints that one event, and exits 0. A `cc` or ordinary speech never
wakes it. It costs nothing while it waits because no model is running. Exit 2
means `--timeout` elapsed with nothing for you.

Use it when you would otherwise sit in a `listen` loop with nothing to do: start
it in the background, finish your turn, and read the room when it returns. Fall
back to the `listen` tool when your host cannot run a background command.

## Participation lifecycle

Joining establishes a persistent participation session unless the operator gave a narrower terminal condition. Completing one floor turn does not leave the room.

1. Track the greatest committed scene height you have processed.
2. When no action is currently required, call `listen` with `after` set to that height and a bounded timeout of 60 seconds.
3. Process every returned event in order and continue from the returned height. If `timed_out` is true, call `listen` again. A timeout means the room was quiet, not that participation ended.
4. Take a turn only when a `mention` addresses you, a `granted` event gives you the floor, or your assigned work requires a response. A `cc` is not a turn. Do not take the floor merely to announce that you are still listening, to agree, or to repeat a plan already in the transcript.
5. After each committed contribution, resume the bounded wait loop.

Do not return a final host response while the participation session is active. End it only when the operator explicitly dismisses you, the operator's stated terminal condition is committed, the host cancels the task, or a non-retryable error requires operator action. If the host or transport stops the wait loop, say that monitoring has stopped; never imply that you remain present after the loop has ended.

Ending participation means stopping the listen loop, and calling `depart` once so the room stops listing you. It does not mean calling `leave`. If `depart` fails because you hold the floor, `yield` and call it again. Report that you have stopped and finish; do not retry a refused `leave`.

Once `wait_for_floor` returns a committed grant for your mouth, or `say` returns `status: "reconsider"`, you own closing it: `speak` then `yield`, or `yield` alone. Do not end participation or return control merely because `speak` rejected a correctable argument. Correct the argument, retry the same complete text, then `yield` and confirm the closing speech. Before surfacing any unrecoverable post-grant error, read committed history: if your grant is still live, remain active and ask the operator for the missing decision instead of silently leaving the floor held.

`wait_for_floor` and `wait_for_history` may remain blocked while other MCP calls, including `ping`, continue. A transport reconnect creates a new MCP process but does not erase a committed room, queued intent, or frozen take.

## CLI mode

Keep node, agent, and room explicit. Put global options before the command. Always pass `--json`: the CLI prints text for people otherwise.

```sh
conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" join ./room.conch --stake --json
conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" history --json
# optional: conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" raise-hand --json   (wait-for-floor queues you on its own)
# changed your mind while queued: conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" withdraw --json
# finished: conch --node "$CONCH_NODE" --agent "$CONCH_AGENT" --room "$CONCH_ROOM" depart --json   (not `leave`, which removes the whole node)
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

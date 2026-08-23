# Review: agent-room design spec (2026-08-23)

Target: `2026-08-23-agent-room-design.md` (671 lines). Five independent reviewers, each reading the whole doc with a different primary lens (invariants/decisions; wrap/ledger/schemas; roster/stake/auth; floor/messages/transports; CLI/UI/failure/examples). Findings merged, deduped, and each quote re-verified against the text. Line numbers refer to the spec. Nothing here reopens a §24 decision.

Convergence note: findings B1, B2, B3, M1, M4, M5, M9, M10 were raised independently by 3–5 of the 5 reviewers. Treat those as settled, not opinion.

---

## Blockers (implementation cannot proceed on §23 steps 1–3 without a decision)

### B1. Sign-once vs abandon-and-repropose: either permanent freeze or two heads
- L306 / L322: "Nodes must refuse to sign a second scene for an `n` they already signed" / "Verify you have not signed a different hash for this `n`."
- L317: "If honest nodes split signatures 50/50 … abandon after `WRAP_TIMEOUT` (default 60s) and allow a new proposal for the same `n`."
- L338: "A cert already made still counts if that node dies afterward. The signature is a piece."
- L631 (Ex. F): "After heal, one pending abandoned by timeout, first well-formed proposal on the unified view wraps."

Reading A (L306 absolute): in Ex. F all four stakers have signed for `n`; nobody may ever sign anything else at `n`; room frozen forever (view-change is also a scene at `n`). Ex. F is unreachable.
Reading B (abandon releases the lock): roster `[A,B,C,D]`, P1 certs `{A,B}`, P2 certs `{C,D}`, heal; A signs P2 → `{C,D,A}` wraps; D signs P1 → `{A,B,D}` wraps. Two majority-certified scenes at `n` from honest nodes; invariant 1 (L116) broken. Also `WRAP_TIMEOUT` has no defined start point (proposal `ts`? first-seen?), and "first well-formed proposal on the unified view" is undefined since first-seen is per node (L420).
There is no round/attempt field in the envelope (L192–210) to distinguish a re-proposal from the abandoned one.

Fix: add a hashed `round` (per `n`) to the envelope; the sign-once lock is per `(n, round)`; a node may sign a higher round only after `WRAP_TIMEOUT` from first-seen of the lower round and only if it has seen no majority for any lower round; certs from lower rounds never complete a scene once a higher round is signed (Paxos-style promise, or accept that even splits freeze until human re-genesis and delete Ex. F).

### B2. Rival grant proposals are the normal case, not the partition case; no proposer election
- L420: "Nodes propose a grant"; L422–423 target selection; L317 first-seen rule; L199/L205/L213 `author`, `ts` are hashed.
- L423: `reason=vacant` targets "some agent with a blocking `wait-for-floor`" — but `wait_for_floor` is node-local (no swarm message in §15), so only the hosting node can know it.

When a speech wraps, every staker with a waiter/queue view proposes the next grant; proposals differ in `author`/`ts`/`floor.raised` so hashes differ; each proposer signs its own first → 1/1/1 with three stakers, 2/2 with four, every turn. Combined with B1 that is unrecoverable. Ex. A's "2 of 3 certs" silently assumes a single proposer.
Fix: designate the grant proposer deterministically (e.g. the node hosting the target mouth; or lowest caught-up roster id with fallback after `WRAP_TIMEOUT`), or make grant proposals canonical so rival proposers hash identically; define `author` for grants.

### B3. Moderator mode has no surface
- L433: "Only that agent (from its node) may propose `grant` or yank." L402 yank.
- L497–509 client messages and L555–567 CLI/MCP: no `grant`, no `yank`, no `membership`. L513 defines `not_moderator` with no verb that could raise it. L556 `create --mode moderator` exists but nothing sets `floor.moderator` (L228).
As written the mode can only ever be vacant.
Fix: add `grant {room, agent, node}`, `yank {room}`, `membership {room, stake?, floor?}` to §15/§17, plus `--moderator` on create.

---

## Major — wrap / ledger / verification

### M1. Canonical-head walk verifies less than Sign does (invariants 1,2,5,7,10 only hold for online signers)
- L304: accept `n+1` iff `parent` matches "and certs meet majority of that scene's `roster`" — trusts the envelope's own `roster`.
- L321 Sign additionally checks "roster equals committed roster, proposer is allowed to propose this type."
A leecher (Ex. C, L625) served a scene with `roster:[X]` and one self-cert, or a grant while another is live, or a bad `next_roster`, accepts it; a full-rule implementation rejects → divergent heads (inv. 10). Poison gate "local chain verifies" (L119) is therefore underspecified.
Fix: §9.5 acceptance = every Sign rule (roster derived from genesis + wrapped view-changes, one live grant, `next_roster` arithmetic, genesis rules); failure → sick.

### M2. "Already signed for `n`" is not persisted → honest equivocation after crash
- L310 state is in-memory; L455–470 disk layout has `head`, `scenes/`, `blobs/`, no pending/signed record. L595 declares crash-fault model.
Node signs P1, crashes before commit, restarts at head `n-1`, sees P2 first, signs it. Two heads within the declared fault model. §20 has no "restart mid-sign-off" row.
Fix: fsync `rooms/<id>/pending/<n>-<hash>.json` (scene + own cert) before emitting `cert`; Sign step 2 consults it; add §20 row.

### M3. "Quorum head", "healthy", "sync" never defined; poison gate not mechanical
- L74, L119, L314 ("see sync" — no sync section). Only observable is unsigned `have` (L485) on unauthenticated `hello` (L482).
Reading A: behind = any reachable peer advertises higher; Reading B: majority advertises higher. Ex. F has minority sides proposing, implying A, never stated. Any token holder advertising `n=10^9` freezes the room.
Fix: gate = (chain verifies per fixed §9.5) AND (no connected peer has *delivered* a verifiable wrapped scene with `n > head`); advertisement alone never sickens; drop or define "quorum head".

### M4. Envelope `floor` and `author` have no semantics but are hashed
- L200–204 `floor:{mode,holder,raised}` never explained: pre- or post-scene state? `holder` when vacant? `raised` is per-node first-seen (L420) baked into a chain hash. L321 does not list `floor` among signer checks.
- L199 `author.agent` undefined for genesis, non-holder timeout close (L400), vacant view-change (L370).
- Genesis envelope (`author`, `floor`, `ts`) unspecified although L151 pins its hash in the ticket.
Fix: define every envelope field per body type (value, nullability), state signers do not validate `floor`, or drop `floor` from the envelope.

### M5. Optional/null/absent, unknown keys, hex case not pinned → same logical scene, different hash
- L185 "without the `certs` field": key deleted vs `"certs": []`? `blobs` when none, `raised` when empty, `closes_grant`/`token_sha256`/`moderator` null-vs-absent: shown but not mandated.
- Unknown fields: L160 covers tickets only; a struct-roundtripping Rust impl drops unknown keys and changes the hash.
- L185 lowercase hex only "in tickets and magnets"; L187 node id case unconstrained; `roster`, `parent`, `closes_grant`, `room` are hashed and L294 sorts by node id.
Fix: "hash = JCS of received object with `certs` key deleted; optional fields present-and-null / present-and-empty (pick one); unknown keys rejected at Sign; all hex everywhere lowercase, byte-wise compare."

### M6. `token_sha256` input ambiguous
- L140 token is "hex, 32 bytes"; L172 `sha256(token)`; L599 "Compare SHA-256". Raw 32 bytes vs 64-char hex string → cross-impl `auth` always fails.
Fix: "SHA-256 over the 32 raw bytes, lowercase hex."

### M7. `closes_grant` nullability and close side effects inconsistent
- L276 membership `closes_grant: "<hash>"` (not nullable) vs L433 membership "proposed when vacant".
- L287 view-change `hash-or-null` but L332 Commit closes grant only "if speech/breakout/membership". L117 inv. 2 says closed "by a speech scene".
Fix: `closes_grant` nullable for membership and view-change, null legal iff no live grant at `n-1`; Commit closes for any non-null `closes_grant`; define "live" = no later wrapped scene of any type names it.

### M8. Genesis cannot be produced by §10 Propose (L314–315: poison gate, `parent = head.hash`). Minor special-case paragraph needed.

---

## Major — roster / stake

### M9. View-change-without-grant rule self-contradictory; precondition impossible
- L296: allowed "only when the floor is vacant **and** the floor is frozen because majority cannot be reached for a grant … or any staker proposes it when vacant." L370: "or when vacant." L336: removal "needs majority of the current roster" — same majority the grant needs, so "frozen" never helps; signers cannot verify "frozen". Ex. A's two adds (L621) happen vacant, not frozen.
Fix: delete the frozen clause; "view-change with `closes_grant=null` is legal iff no grant is live."

### M10. Signers cannot verify add/remove "justified by stake policy" (L294)
- Justification depends on local observations (L352–356: attached agents, `role`, caught up, connected; L370: disconnected for `REMOVE_AFTER`) carried in no swarm message (`hello` L482: node, pub, addrs, rooms — no role, no agents). Signer A still sees X, rejects the `remove` B signs. "Policy no longer requires it" is ambiguous for a disconnected node (not connected ⇒ never eligible ⇒ remove everyone after 300s?).
Fix: split into signer-verifiable structure (`next_roster` arithmetic, non-empty, added node completed `hello`+`auth`, allowlist, a self-signed role/agents declaration gossiped in `hello`) and proposer-only heuristics (liveness, 300s).

### M11. Policy composition undefined
- L348–356 "combinable" with "should"/"must not"/"must … even if" and no operator. AND vs OR for `agents`+`explicit`; what `{false,false,[]}` means; what `{false,false,[X]}` means.
Fix: one boolean formula, e.g. eligible = role≠observe ∧ (allowlist=[] ∨ node∈allowlist) ∧ ((agents ∧ has_agent) ∨ (explicit ∧ role=stake)).

### M12. Default `join` role and auto-join role unspecified
- L501 `role: stake|observe`, L556 `[--observe|--stake]` no default; L449 auto-join "runs join" with no role; L362 "nano must join --observe" implies stake default. Under default policy `explicit=true` the default decides eligibility. Ex. D depends on it.
Fix: default `stake`; auto-join always `stake`.

### M13. `create --observe` can never wrap genesis
- L556/L500 `--observe` on create; L366 roster `[creator_node]`; L221 genesis needs creator cert; L124 observers do not sign; L372 empty roster illegal.
Fix: remove `--observe` from create, or creator is always a staker until view-change removed.

### M14. Non-roster nodes proposing
- L316 Propose = "Sign. Gossip Propose + own Cert" — meaningless for a non-roster node, yet L370 has "the node itself" proposing its own add, L427 allows grants to agents on any caught-up node (incl. observers), and an observer-hosted moderator/holder must `yield`/`grant` from a node that cannot sign.
Fix: state whether `propose` may originate from a non-roster node (recommended yes; proposer's own cert ignored) and whether observer-attached agents may hold the floor.

### M15. `leave` has no semantics (L508, L564). Self-remove? Auto-yield if holding (else floor frozen 1800s)? Two-staker caveat (L378). L472 key replacement has the same hole.

### M16. Breakout details
- L447: child ticket `trackers`/`peers` unspecified; empty ⇒ auto-join fails per L641; nano does not join child (L627) so cannot track it.
- Child genesis lives only on the holder (L176, L463); breakout body embeds the ticket (hash), not the genesis scene — holder dies after wrap ⇒ stillborn child. Child stake/floor policy derivation unspecified.
- L332 side effects on catch-up replay: a late leecher of the lobby auto-joins every historical child (and rooms it left)? Or only live wraps — then an offline listed node never auto-joins.
Fix: embed full child genesis; populate `peers` from holder's `listen.json` + lobby addrs of `auto_join`; state catch-up applies only deterministic state (roster, live grant, policy), auto-join fires once when a node first reaches head ≥ breakout scene.

---

## Major — floor

### M17. Example A violates the grant rule
- L621: "Codex `raise-hand`, Claude `wait-for-floor`. Grant to Claude wraps" vs L422: queue nonempty ⇒ `to` = head of queue = Codex. Also implies Codex later has a `wait` open without saying so, and never says whether `wait_for_floor` implicitly raises (skill text L575 tells agents to do both).
Fix: `wait_for_floor` implicitly enqueues a signed `raise` for `(agent,node)`; fix Ex. A; state that `to`/`reason` selection is proposer-local and signers check only mode/moderator/one-live-grant.

### M18. Raise-hand lifecycle undefined
- L420 "FIFO by first-seen `(ts, agent, node)`" — first-seen order and ts-tuple order are two different orders. No nonce/expiry; never said when an entry leaves the queue (grant wrap? close? leave?); a replayed old `raise` re-queues at the head; an agent that raised but no longer waits gets a grant and wedges the floor 30 min.
Fix: order by local arrival; dedupe by `(agent,node)` keeping max `ts`; consumed when a grant to that pair wraps; ignore `ts` older than last grant to that pair; grant only to a mouth with an open `wait_for_floor`.

### M19. Non-holder close (timeout / "holder is gone") unverifiable and clock-dependent
- L400 "any staker if the holder is gone"; L615 "another staker empty-closes"; L402 yank only in moderator mode; L213 "wrap does not depend on clock agreement"; L321 signer checks proposer permission.
"Gone" undefined; timeout measured from a wrap time not in the ledger; signers with skew disagree; a non-holder close can race a live `yield` and win, losing the take; §20 reading allows a free yank in stick mode.
Fix: signer rule, e.g. non-holder speech closing G valid iff `scene.ts - G.ts >= timeout_secs` (chain-visible advisory ts, same answer everywhere) or proposer is moderator; `author.agent` reserved `room:timeout`; "gone" is not a condition.

### M20. `attach` / "attached" undefined
- L499 `attach {agent, token?}`; L553 `--agent NAME` per CLI invocation (new TCP connection per command). Per-connection reading detaches the agent between `wait-for-floor` and `speak`, breaks L427 and churns roster via L352. `attach.token` meaning undefined.
Fix: node-side registry keyed by agent, kept across connections, removed by `leave` or idle TTL; define or drop `attach.token`.

### M21. `yield` / `speak` return semantics; abandoned speech
- L569 `speak` prints `n` but creates no scene (L396). L569 "pending or committed n" lets `yield` return before wrap; if the speech is abandoned (L336) nobody re-proposes and the agent has already moved on; buffer fate unspecified (L437 covers drafts only).
Fix: `yield` blocks until wrap or `timeout` (buffer retained, grant still live, node auto-retries); drop `n` from `speak` or define as grant `n`.

### M22. `membership` proposer authority unstated (L433). Non-moderator holder can switch back to stick as their take; any staker when vacant; moderator matched by agent id alone (L431) while everything else is `(agent,node)`. No recovery if moderator node is offline.

### M23. `speak` not behind poison gate (L119 lists sign/propose/history only; L396). Minority-side holder keeps getting `ok` while the majority closes its grant; text silently lost. Fix: `speak` requires gate → `sick`; `yield` after remote close → `no_grant`.

---

## Major — protocol / surfaces

### M24. CLI has no room selector (L550, L558–565) though every client message takes `room` (L502–509) and nodes are multi-room (L32, L589). No verb lists rooms; no defined way for a lobby agent to learn the child id. Fix: `--room`/`ROOM_ROOM` with default, `room list` or `status.rooms`, child discovered via breakout scene in `history --follow`.

### M25. `create` cannot set `token`, `trackers`, `peers`, `timeout_secs`, `allowlist`, `moderator` (L556 vs L138–150, L159, L228).

### M26. Blobs have no client path (L252 vs L504 `speak {room,text}`, L560 `--file -` ambiguous: stdin-as-text or stdin-as-blob). Either add `--blob PATH` + client blob frame, or state blobs are not client-settable in v1.

### M27. No message forwarding rule. `propose`/`cert`/`raise`/`draft` (L476–493) direct-only vs flooded? A staker reachable only via the nano can never contribute to majority under direct-only; flooding needs dedup keys `raise`/`draft` lack. "Mesh among reachable pairs" (L539) does not settle it. Fix: flood-with-dedup, observers relay too; `draft` = full buffer + `seq`.

### M28. Client protocol has no request id / subscription framing (L511, L515, L517, L579). One WS with `history follow` + outstanding `wait_for_floor` cannot correlate. No `proto` version in `hello`/`attach`.

### M29. `join` reply returns a syncing prefix to agents (L519) — contradicts invariant 4 (L119: "does not return `history` to agents. UI may show verified prefix"). Also `history` on a sick node: error vs block vs prefix is unstated for the shared client protocol. Fix: `history` → `{ok:false, code:"sick"}` for agents, `verified_prefix` option for UI; `join` returns `{syncing, head}` only, or amend inv. 4.

### M30. Magnet form has no `genesis` hash (L165) but join step requires it (L174) and re-genesis (L176) makes it matter. Fix: `x.genesis=<hex>` required, or state magnet joins skip the check.

### M31. Which swarm messages `auth` gates, and ordering, unstated (L483, L539, L599). Fix: all room-scoped messages rejected `unauthorized` until `auth`; genesis fetch is the exception.

---

## Major — build order / tests

### M32. Build order dependency inversion (L651–655). Step 2 "3 nodes … wrap" needs view-change (step 4, roster starts `[creator]` L366) and `join`/ticket (step 5). Move minimal TCP `create`/`join` + view-change into step 2.

### M33. Tests (L637–645) miss: view-change add/remove + `next_roster` rule; timeout / holder-dies close; moderator + yank; membership switch; token `unauthorized`; ticket/genesis mismatch refuse; **post-heal recovery after 2-2 split** (L637 only tests "no second head"); two-stakers halt; blob completeness under gate; "behind head ⇒ does not sign".

---

## Minor

- `unavailable` (L617) not in error list (L513); only `timeout→exit 2` mapped (L571). Publish code→exit table.
- Ticket `floor` (L147) lacks `moderator` and ticket `parent` vs genesis `parent_room` (L229) though L158 says ticket = what genesis claimed.
- L176 re-genesis "needs the room secret key" (same id) vs L378 "new genesis (new room id)". Which?
- L668 "not a *default* voter" vs L358 "never eligible".
- L215/L329/L645: certs grow post-commit ⇒ persisted files and `GET /history` differ per node; say equality is on hashes.
- L614 "Delete bad scene files, re-leech" (imperative to whom?) vs L629 reads automatic; state when full-chain verify runs.
- L509/L565/L586 `status` output undefined though UI floor rail depends on it.
- L487/L488 `scene`-pending and `propose` are two encodings of the same thing.
- L390 wait-for-floor condition 3 vs L332 side-effect list: define "live" once (see M7).

---

## Verdict
Not implementable as written. B1 (sign-once vs abandon) is a safety/liveness contradiction at the core of §10 and must be decided first; B2 and M1–M5 must be pinned before §23 step 1–2; B3 and M17–M21 before step 3. Nothing found contradicts a §24 decision — every fix above is a clarification or a missing rule, not a re-litigation.

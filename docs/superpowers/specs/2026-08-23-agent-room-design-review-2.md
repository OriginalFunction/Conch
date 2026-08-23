# Review 2: agent-room design spec v1.1 — §10–§12 and Example F only

Target: `2026-08-23-agent-room-design.md` v1.1 (749 lines). Scope per author's request: §10 apply(), §11 Consensus, §12 Floor, Example F. Goals and §24 not relitigated. Three independent reviewers (one per section), each reading the whole file; findings merged, deduped, every quote re-verified. Prior review: `2026-08-23-agent-room-design-review.md` (v1).

## v1 blocker status

| v1 | Status in v1.1 |
|---|---|
| B1 freeze-forever | **Closed.** New term permits a new hash at same `n` (L373). |
| B1 two-heads | **Reopened in Raft form** — see R1 below. |
| B2 rival grants every turn | **Closed.** Single proposer per term (L361, L377, L447), conditional on R2. |
| B3 moderator verbs | **Half.** Verbs exist (§16/§18) but have no swarm path to a leader on another node — R3. |
| M1 catch-up ≠ sign | **Closed** in structure (apply() steps 3/6/8/9). Residuals R8, R9. |
| M2 pending not persisted | **Closed** (L332–334, §15). Residual R6 (candidate's own term/self-vote not fsynced). |
| M3 quorum head undefined | **Closed** — `valid` is the gate, `have` never sickens (L385). `Healthy` (L91) is now a dangling definition nothing consumes. |
| M4 envelope `floor`/`author` | **Closed** (L219). |
| M5 bit-exactness | **Mostly closed** (L153–157). Residuals R4 (sig preimage — a regression), R11. |
| M6 token_sha256 | **Closed** (L157). |
| M17–M23 floor | M17 closed but Ex. A trips a new rule (R14); M18 mostly (R12, R13); M19, M20, M21, M22, M23 **not closed** (R15, R16, R17, R18). |

---

## Blockers

### R1. Election ignores certified-but-uncommitted scenes → a new leader can overwrite a scene the old leader already committed. Two heads among honest nodes.
- L347: "Candidate's last **committed** `(last_term, last_n)` is at least as up to date as ours"
- L371: "On majority: attach certs, fsync scene into `scenes/`, delete pending, advance head, apply() side effects, gossip `scene` wrapped"
- L373: "New term: leader may propose a different scene at the same `n` if the previous term did not commit."
- L572: `vote {room, term, voter, grant}` — voter reports nothing about its pending.

Trace, roster `{A,B,C}`, majority 2, crash-fault only:
1. Term t, A leader, proposes S at n. B fsyncs pending (n,t,H), certs.
2. A has {A,B}: fsyncs S into `scenes/`, head=(n,H). A crashes before gossip. S is committed by L221's definition.
3. B, C time out. C campaigns at t+1 with `last_n = n-1`. B's *committed* last is also n-1 → rule 3 passes → B votes C.
4. C proposes S' at (n, t+1), hash H' ≠ H. B's pending is term t, different term → per L373 / Ex. G (L704) B certifies S'. C commits S' with {B,C}.
5. A restarts, replays: head (n,H). B, C: head (n,H'). Invariants 1 and 10 broken; no rule in §11.4 handles committed-vs-committed conflict at the same n.

Variant with no crash: 5-roster, A leader t, A and B certified; C fsyncs pending and its cert is in flight; D campaigns t+1, C votes D (rule 3 ignores pending); D commits S' with {C,D,E}; C's late cert reaches A, still term-t leader (no step-down-on-lost-quorum rule) → A has {A,B,C} → commits S. Two heads.

"If the previous term did not commit" (L373) is not locally decidable. "Certs from other terms do not count" (L373) is vacuous today because `scene.term` always equals the proposing term and the sig is over the hash only. This is Raft's leader-completeness property, dropped by comparing committed logs instead of logs-including-pending.

Fix: `pending.json` = `{n, hash, scene_term, accepted_term}`; a node's tail = `(accepted_term, n)` if pending is at `head_n+1` else `(head_term, head_n)`; `request_vote.last_*` carry the candidate's tail; a voter refuses any candidate whose tail < its own; a newly elected leader that holds (or learns from a voter) a pending at `head_n+1` MUST re-propose that exact hash under its own `append.term` (so `append.term` may exceed `scene.term`); followers certify the same hash and bump `accepted_term`; leader counts only certs with `cert.term == current_term`; only a leader with no pending at n may propose a fresh body. Update L373, L375, L681, L704. Add: a committed scene at n with a different hash than local committed n is refused and logged.

### R2. Leader identity = "the node I voted for" — no rule for a higher-term `append`/`heartbeat` → election churn under the literal reading.
- L353: "Followers treat `leader` as the node they voted for in current_term if it is sending appends with that term."
- L345: term adoption stated only for `request_vote`. L355: "If a follower hears no append/heartbeat from a current-term leader within the timeout, increment term, campaign."

A node that voted for a loser, voted for itself, or missed the election never recognizes the winner; it times out every [3,6]s, bumps term, everyone steps down. Also §12.4 `close_take` "to the leader" is undefined for such a node.
Fix: Raft rule — any message with `term > current_term` adopts it, clears `voted_for`, steps down; `append`/`heartbeat` with `term == current_term` identifies that term's leader regardless of `voted_for` and resets the timer; a candidate receiving one becomes follower.

### R3. Client `grant` / `yank` / `membership` / `breakout` have no swarm message to reach the leader.
- L450–452: leader proposes on moderator's client `grant`/`yank`, on holder's request for membership/view-change/breakout.
- §16 swarm table (L565–581): client-originated requests on the wire are only `intent`, `close_take`, `draft`, `leave`.

Unless the issuing node *is* the leader, the request cannot arrive. Moderator mode and any holder≠leader breakout/config break as soon as leadership moves.
Fix: swarm `request {room, kind: grant|yank|membership|breakout|vacate, payload, node, sig}` routed to current leader (or extend `intent` with signed `kind=grant|yank` from the moderator node).

### R4. Cert / room-signature preimage unpinned (regression from v1).
- L304: "signatures verify over the scene hash"; L189: "Room secret key signs the hash"; vs L431 intents: "node ed25519 over JCS without sig". v1 said "ed25519 over that 32-byte hash" explicitly; v1.1 dropped it.
Raw 32 digest bytes vs 64-char hex vs JCS bytes → two implementations agree on every hash and verify none of each other's certs.
Fix: "cert and room `sig` = ed25519 over the 32 raw bytes of the scene hash; intent/decl/leave sigs = ed25519 over the JCS bytes of the object without `sig`."

---

## Major — §11

### R5. Heartbeat interval undefined; election timer start undefined for candidates/leaders (L351 "must heartbeat before proposing", L355). A heartbeat-only-before-propose implementation deposes an idle leader every 3–6s; candidate retry on split vote only implied. Fix: heartbeat every T/3; timer starts at boot / on becoming follower / on each accepted append or heartbeat; candidate re-campaigns with new term on its own timeout.

### R6. Candidate does not fsync `current_term`/self-vote before `request_vote` (L332 covers only sent `vote`s; L351 "including itself"). Win-propose-crash-restart-at-t−1 then vote for another term-t candidate → two term-t majorities. Sign-once prevents two hashes but stalls t and breaks the one-leader-per-term premise R1's fix relies on. Fix: fsync `{current_term, voted_for: self}` before campaigning.

### R7. Vote rule does not require candidate ∈ roster (L343–347); observers/removed nodes can win a term and burn it (apply() step 4 blocks commit). `last_hash` in `request_vote` (L571) is never used. Fix: voter refuses candidate not in its committed roster; use `last_hash` to refuse a fork or drop it.

### Minor §11
- L334 "re-advertise" has no message (`have` forbidden for pending, L381). Define: follower re-sends `cert` to current leader on each heartbeat; leader re-sends `append` each heartbeat until commit/step-down; pending with `term < current_term` may be deleted; `cert` goes to leader only.
- L368–369 follower rule has no branch for "same n, lower term, different hash" (resolved by L373/Ex. G; write it out as 3b since R1's fix lives there).
- L363 `propose` vs L573 `append`: pick one `typ`.
- L357 / L702 "No propose" / "No grant proposed": a pre-partition leader on the minority side has no step-down rule and may propose (2 certs, no commit). Say "no grant committed".

---

## Major — §10 apply()

### R8. apply() is not one function of `(state, scene)`: steps 7 and 10 are signer-only (L302, L305, L367) but the signature (L292) and the principle sentence (L290 "If catch-up would accept something a signer would reject, apply() is wrong") don't admit a mode. Fix: `apply(state, scene, ctx{mode, intents, blobs})`; label steps 7-intent and 10 as `mode=sign` only; reword L290 to "chain-derivable checks are identical in all modes".

### R9. "Unmaterialized" scene (L305) has undefined effect on ChainState head (L309), `have` (L381), `history` (L607). Committed per L221 but not head per L89. Fix: `committed_n` vs `valid_n` in ChainState; `have` advertises `valid_n`; `history` serves `0..=valid_n`.

### R10. Blob fetch failure at cert time unspecified (L267, L687 "Do not certify"): leader keeps heartbeating so no election; take sits CLOSING forever. Also no message carries the blob list from holder to leader — `close_take` (L577) and `speak` (L592) have no `blobs`, so step 10 is unreachable through defined messages. Fix: `blobs[]` on `close_take`; follower `get_blob` retry for BLOB_WAIT then discard; leader retransmits unacked appends each heartbeat and abandons after PROPOSE_TIMEOUT.

### R11. Body-schema and roster rules not in apply():
- Step 1 (L296) checks envelope only; §9.2 rules (`reason` enum, `to.agent` regex, `intent_id` hex, `auto_join ⊆ roster`, ticket validity, membership shape, genesis `term=1`/`leader=creator`) are unlisted.
- §13.2 "signer-verifiable" allowlist rule (L511) is chain-derivable (stake is in ChainState) but absent from step 8 (L303).
- `reason` (L239) never tied to `floor_mode` — followers either reject `reason=queue` in moderator mode or treat it as decorative; both are apply() divergences.
- Embedded breakout `ticket` (L250) is inside a hashed object: L155 "unknown keys hard error" vs L177 "ticket unknown keys ignored" — pick.
- Genesis `room` cert required by L234 but not by step 9 (L304).
Fix: step 1 includes §9.2 schema per type; step 7b `reason == moderator iff floor_mode == moderator`; step 8 adds allowlist; step 9 adds n=0 room-cert rule; "ticket inside a scene is hashed: unknown keys reject".

### Minor §10
- `blobs: []` neither "omit" nor valid (L244 vs L155): reject `[]`.
- Hex case: "lowercase" is not a verb — reject non-lowercase at step 1.
- Integer-ness pre/post JCS (`1.0e9`): require no fraction/exponent in received bytes.
- `name` 1–128 "chars": bytes or scalar values?
- ChainState lacks `room` and `genesis_hash`; startup replay (L555) never re-checks ticket `g`.
- View-change: remove ⊄ roster / add ∩ roster ≠ ∅ pass the arithmetic (L261) — state whether they're legal.
- `Healthy` (L91): delete or make informational-only with "recently" defined.
- `append` carries `certs: []` or omits the key? (L202 vs L573.)
- Ex. E L700 "errors `sick` or returns last valid prefix" — pick (L607 says prefix).

---

## Major — §12 floor

### R12. Intent expiry starves queued waiters. `exp` default `ts + timeout_secs` (L435); floor times out at `commit + timeout_secs` (L415). Second-in-queue expires before its turn whenever a take runs long; a blocked `wait-for-floor` has no renewal rule and renewing with a new `ts` loses its place (L437). Fix: node auto-renews a blocked waiter's intent keeping original `ts`, or order by first `ts` per mouth.

### R13. Intent replacement semantics wrong (L435 "old one remains until expiry; leader grants the earliest"): the old intent is earliest so "replace" does nothing; after it's granted and closed the new one is still unconsumed → same mouth granted twice, floor wedged. Fix: newer intent voids older for that mouth (leader and followers treat older as invalid).

### R14. `wait-for-floor` "sends an intent if none exists" (L419) trips Ex. A (L692): after the grant to Codex commits (I1 consumed), Codex's `wait-for-floor` finds no unconsumed intent and sends I2 with earliest `ts` → Codex re-granted after its own speech, contradicting "Claude's later raise is next". Fix: send only if no unconsumed intent for this mouth AND live grant is not to this mouth.

### R15. Timeout/"holder is gone" still underspecified (L403, L451, L415): unconditional at `commit+timeout_secs` vs only-if-gone; on whose clock (new leader measuring from its own observation waits a full extra period; using grant `scene.ts` closes at once); "no buffer" — leader isn't the holder, so its only text is the unverified `draft` (L473): commit draft under the holder's name or `""`? Fix: leader empty-closes when `now ≥ grant.ts + timeout_secs` (proposer-only); text empty unless holder delivered `close_take`; yank asks holder to freeze and waits bounded time for `close_take`.

### R16. CLOSING take with abandoned/lost proposal (L395, L403, L375): leader steps down or dies before `close_take` lands — does the holder node re-send to each new leader? Does `yield` return on freeze or block to commit, and what does it return if another scene closes the grant? `speak` has no `valid` requirement (L395) so a minority-side holder keeps getting `ok`. Fix: holder retries `close_take` on every leader change until `closes_grant == grant_hash` commits; `yield` blocks to commit (returns `n`) or `timeout` with take still CLOSING; closed-by-other → `no_grant`; `speak` requires `valid`.

### R17. Follower cert rules for grants ambiguous: queue order (L437/L449) is not listed in L439 but L367 says "would this be signable" — a strict follower with an earlier intent refuses; "not expired" (L439) on follower's wall clock vs `scene.ts`; followers that missed the gossip have no `get_intent` and `append` doesn't carry the intent (L573) — liveness gap exactly where the safety check is. Fix: state queue order and moderator consent are proposer-only; followers check `scene.ts ≤ intent.exp`; `append` for a grant carries the intent object; add `get_intent`.

### R18. `membership`/config authority unstated (L452, L598, L647): in moderator mode any agent can flip mode to stick when vacant; no dead-moderator recovery; CLI `config` vs client `membership` naming. Fix: moderator mode → only moderator mouth (else `not_moderator`); stick → holder or any staker when vacant; name the recovery path.

### Minor §12
- Take state/buffer/`request_id` table not on disk (L391–405, §15): on holder restart with live grant, OPEN-empty or CLOSED? Say which.
- No "lower hand" / intent withdrawal; client `--timeout` leaves intent queued till `exp`. `leave --vacate` and `attach {agent}` have no semantics.
- `kind: raise|wait` is signed but never used (L419, L427).

---

## Example F trace (L702)
| Sentence | Follows? |
|---|---|
| Neither elects a leader (need 3) | Yes (L94, L351) |
| No grant proposed | No — pre-partition minority-side leader may propose; say "no grant committed" |
| Heal. Election. | Yes (L345 rule 1) |
| One leader | Only under Raft reading of L353 (R2) |
| One grant | Yes (L361/L373/L704) |
| No second committed hash at that n | Yes *for this example* (nothing committed during split). General claim fails per R1. |

## Verdict
Structure is right and most of v1 is closed. Three things still stop a build of §23 step 2–3: R1 (leader-completeness — a real two-heads trace inside the crash-fault model), R2/R5 (leader recognition + heartbeat, or the cluster won't hold a leader), R3 (no wire path for moderator/holder requests). R4 is a one-line regression that would make two implementations mutually unverifiable. The rest are pins, not design.

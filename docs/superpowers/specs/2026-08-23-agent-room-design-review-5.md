# Review 5: agent-room design spec v1.4 — §8.1 cert preimage, durable commit order, Examples H/H2

Target: `2026-08-23-agent-room-design.md` v1.4 (900 lines). Author's question: do §8.1's cert preimage, the durable commit order, and Examples H/H2 still allow two committed hashes at one `n`? Two independent provers (adversary; implementer) plus my own pass; every quote re-verified. Prior: `-review-4.md` (v1.3).

## Answer: No.
Neither prover could construct two committed hashes at one `n` among non-equivocating crash-fault nodes, and my own trace agrees.

- **Preimage (L155–159, L321, L461–464):** every commit cert signs the digest of `{room, n, hash, rpc_term, leader, node}`; `leader` is the proof assembler and may differ from `scene.leader` (stated three times, consistently). Genesis node certs use `rpc_term=1, leader=creator`; the room key signs the scene hash. A cert cannot be counted by another leader or term. Term-binding is now cryptographic, not convention.
- **Durable order (L374–381):** the scene file is fsynced before `pending.json` is unlinked and before `commit` is pushed, so a node's tail never drops below a value it helped make majority-accepted while that value is uncommitted. Every crash between steps 1–4 and at startup was enumerated; none yields a second hash.
- **Best attack found (and why it dies):** node N with pending H′@n receives committed X@n (L369), writes `scenes/<n>-X.json`, crashes before unlinking pending. On restart L370 reloads pending; L381 forbids only *inventing* pending for the committed hash, so N's tail becomes `(t′, n, H′)` while its head is X@n. If N wins it re-appends H′ with `prev=(n,X)`. Fails: X's certifiers (a majority) hold tails ≥ `(tx, n, X)` and refuse N (L392); any normal follower rejects on `prev` (L456) or step 2 (L314); and N's own `apply(commit)` of H′ against head X fails step 2, so no proof is ever pushed. Liveness wedge, not safety (see E3).
- **Example H (L843):** every sentence follows — E's term-3 `request_vote` bumps B/C/D (L449) and is refused (L392); B campaigns at 4; C/D grant on equal tails; probe finds n−1 → carry-forward (L441); C/D fire L470/L364 and re-sign for (4, B); commit (L482); push (L484); A's return is idempotent (L484, L321).
- **Example H2 (L845):** L438 fires (C/D `have.n` = n > B's head n−1) and converges with L439/L496 on "same hash → commit and clear pending"; no re-append. Different-hash variant: L438/L496 install X, and B could not have won anyway (X's majority refuses B's lower tail).
- Also confirmed: no path replaces a majority-accepted value (L366/L473 + L392); one leader per term survives single-change rosters (`majority(k)+majority(k±1) > voters`, L398); L400's "never recedes" holds by definition of a valid proof.

The leftovers are liveness/executability pins. None reopens safety.

---

## Major — liveness
### E1. Follower-side catch-up has no trigger; `have` has no cadence or request; no append-reject signal.
- L438–441 run only at Win, for the leader; L456 "Reject append if they do not match" is silent; L484 push is step 4 of the *committing* leader only; L498 says a higher `have.n` "does not move head" and nothing else; L436 "probe `have`" has no request message and no wait bound; §16 has no nack; when followers send `have` is unstated.
- Wedge with majority alive: {A,B,C}; A commits X@n with A+B, pushes to B only, dies; B wins term 2; appends n+1 with `prev=(n,X)`; C (head n−1) rejects forever; B's heartbeats keep C's timer reset. Also: returning A in Example H (head n, term-2 proof), elected later, heartbeats `(n,H)` and appends n+1; C/D with pending H reject forever.
Fix (one paragraph in §11.5): every node sends `have` after `hello`, after every commit, and once per heartbeat interval; any node seeing `have`/`heartbeat`/`commit`/`append.prev_n` naming a committed `n > head` sends `get_scenes(head+1..n)` to that peer every 500 ms; a follower rejecting an `append` replies with its `have`; the probe at Win waits one `have` per connected roster member or 500 ms; a `commit`/`scene` whose proof verifies is applied regardless of `rpc_term` vs `current_term`; `commit` with a gap triggers the same fetch.

### E2. A leader that commits its own removal steps down before it finishes the push.
- L404 "After a view-change removes this node, it steps down … does not campaign" vs L379 "Only then send `commit`" and L484 "Retransmit `commit` … until … a higher term appears".
- Roster 2: survivor needs `majority(2)=2` from the *old* roster and the removed leader won't vote → both alive, permanent halt. Larger rosters: followers campaign, the higher term ends L's retransmit before peers have the scene.
Fix: "a leader that commits its own removal keeps heartbeating and retransmitting that `commit` until every reachable roster peer's `have` is `{n,hash}`, then steps down."

### E3. Stale `pending.json` at startup (crash between L376 and L378, or the L484 same-hash no-op path).
- L370 "reload pending"; L381 only "do not invent pending for a hash that is already a committed file"; L386 tail = pending if present.
- Same-hash: tail at the wrong ballot; a restarted leader with stale same-hash pending that wins re-appends with `prev=(n,H)` to n−1 followers → rejected, slot occupied. Different-hash (L369 path): the attack above — cannot win against X's majority, cannot commit, but violates L372's one-slot rule and confuses elections.
Fix: "on startup and on every commit at n, unlink `pending.json` if `pending.n <= head.n` (any hash); tail uses pending only when `pending.n == head.n+1`."

## Minor
- Step 9 (L321) vs L157/L239: the genesis `room` entry's digest is the scene hash (not the cert payload) and must not count toward `count >= majority`; step 9 as written verifies it over the wrong preimage and counts it. One sentence.
- Startup scan (L377/L381) vs L821 "Corrupt scene file … sick": a torn file from a crash mid-step-1, or a `staged` file (no location in §15) that fails step 10, reads as corruption → node sick after an ordinary crash. Fix: write temp + rename; non-verifying file above the valid prefix is discarded, not sick; `staged/` directory replayed in `staged` mode; two verifying files at one n = fatal halt.
- `leader_id`: persisted in L348 (fsynced, L345) vs L370 "unknown until `append`/`heartbeat`" vs L677. Resume-as-leader or follower — both safe; resume skips the L436 probe. Declare volatile.
- `vote.last_*` (L411–421): voter's tail or echo of candidate's? L434 never reads them. Say "echo".
- No randomized election timeout in §11.3 (L406/L451/L445 T=3s only); Example H implicitly relies on E and B timing out at different instants. Add `[T, 2T]`.
- `head { n, hash, scene_term }` (L350): `scene_term` is read by no rule; tail uses `commit_proof.rpc_term` from the scene file. Mark `head` optional `{n, hash, rpc_term}` or drop.
- `commit` receiver (L484/L706): say it recomputes the scene hash, checks `hash/n/room` against `scene`, and rebuilds `{rpc_term, leader, certs}` before step 9; `n > head+1` → fetch (E1).
- Cert payload (L461) gives key names only; show one filled example as L411–421 does for `vote`.
- §20 wording: L817 "Unused vote" → cert; L819 "Same hash at that scene.term" → `rpc_term`; L818 add "or installs a peer's proof".

## Review-4 status
D1 closed (L155–159/L321/L461–464). D2 closed for the leader (L436–443, H2) — follower side is E1. D3 closed (L392). D4 closed (L404) — new corner is E2. D5 closed (L470). D6 closed (L424–430, L702). Durable order closed on the happy path — startup corner is E3. Minors carried: `leader_id`, `head` file, §20 wording.

## Verdict
§11 v1.4 is **safe under crash faults**: the cert preimage, the durable commit order, and Examples H/H2 admit no second committed hash at an `n`, and the core machine (accept/cert/carry-forward/in-place commit, tail, election, probe) is implementable from the text. Three liveness pins remain (E1 follower catch-up + `have` cadence + reject signal; E2 self-removing leader; E3 stale pending at startup), each one paragraph or less. Nothing here reopens a design decision.

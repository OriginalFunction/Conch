# Review 6: agent-room design spec v1.5 — `advance_term` and Example I

Target: `2026-08-23-agent-room-design.md` v1.5 (943 lines). Author's question: with `advance_term` on live catch-up, can S still win term 101 and commit H′ while A holds committed H? Two independent provers (adversary; implementer) plus my own pass; every quote re-verified. Prior: `-review-5.md` (v1.4).

## Answer: No.
All three traces agree. Argument on stated rules only:

1. Let t be the `rpc_term` of A's proof for H@n; its certs come from a majority M_H of the roster at n (L228, L323), and `scene.roster == state.roster` (L317) means every leader appending at n — A, and the term-101 leader that gave S its H′ — counts votes over the *same* roster. Each member of M_H accepted an append at rpc t, so had `current_term == t` at that moment (L489 "reject stale", L482 bump) and a tail ≥ `(t, n, H)` (L383, L406–411).
2. S winning any term T needs a majority M_S; M_H ∩ M_S ≠ ∅. If x ∈ M_H certified H before voting for S: x's tail `(t, n, H)` vs S's `(101, n, H′)` — if t > 101 refuse on higher `last_rpc`; if t == 101 impossible (one leader per term, A ≠ L101); if t < 101… see 3. If x voted for S first: x's `current_term` ≥ T and A's append at t < T is stale (L489) — x ∉ M_H.
3. What v1.5 actually closed: Codex's fork needed A to win a ballot (2) *below* ballots a majority had already promised. With the floor, A's first campaign after installing P@n−1 (proof 100) is at `max(100,100)+1 = 101` (L432). The term-101 leader that produced S's H′ needed a majority M1 of the same roster; M1 ∩ {A's voters} ≠ ∅ → that node is at ≥101 with `voted_for` set (cleared only by a strictly higher floor, L367) → refuses A under L465 rule (2) or (1). **A cannot win 101.** A retries upward (L484); whoever wins ≥102 either carries forward H′ (S wins; H never exists) or A wins with no pending and mints H at ≥102; then S's tail `(101, n, H′)` loses on `last_rpc` to every H-certifier (L413 first clause). S never assembles a majority. One head.
4. Ballot reuse across roster churn: a far-behind clique with an old roster R(m) can't elect and commit at its height — scene m's cert majority *is* a majority of R(m) (L317), so some clique member already certified the real m and refuses (or carries the same hash forward). L419's consecutive-roster intersection is needed only for a leader elected with a view-change pending; `majority(k)+majority(k+1) > k+1` holds.
5. `advance_term` itself (L363–373): strict `>` means a leader's own commit never deposes it; `voted_for` is cleared only on entering a term the node could not yet have voted in; `current_term` never lowers; a node at floor 100 with `voted_for` omitted cannot grant a second term-100 candidate, because rule (3) forces that candidate's `last_rpc ≥ 100` and L432 then forces it to campaign at ≥101.
6. `max(current_term, tail.last_rpc)` in L432/L484 is defensive, not load-bearing, once the floor holds (`pending.accepted_rpc_term ≤ current_term` by L489; head proof ≤ `current_term` by L360). Keep it.

**Example I (L884)** follows for sentences 1–4, but sentence 5 — "S at 101 cannot beat a tail that accepted H at 101" — names the wrong mechanism. Nobody accepts H at 101 (101's leader is S's source). The operative rules are vote-quorum intersection at 101 pushing A to ≥102, then L413's higher-`last_rpc` clause; the equal-rpc-equal-n hash-refuse branch is unreachable here (it is only the equivocation guard). A tester following the example as written tests the wrong branch.

---

## Findings (no blockers; nothing bears on one-hash-per-n)

### Major
- **Win sequence has no abort on mid-probe demotion** (L469–474, L371). If the probe installs a proof with `rpc_term > current_term`, `advance_term` makes the node a follower, but the Win path (probe → install → carry-forward) has no abort clause — L384's "until … higher term" covers only the append loop. One engineer continues and appends at the new term with `leader=self` without having won it; another aborts. Fix: "abort Win/carry-forward immediately if `advance_term` changes role; re-enter as follower."

### Minor
- **Example I wording** (L884): reword to "A's `request_vote` at 101 is refused by a node in L101's quorum (`voted_for`, L465); A wins ≥102, H is accepted at ≥102; S's `(101,n,H′)` loses on `last_rpc` (L413)". Add to §22 L891: "leader of term 2 that installs a term-100 proof steps down and never appends at rpc 2; A refused at 101; S at 103 refused on `last_rpc`."
- **Example G** (L878) "A candidate at n−1 loses" — only for tails `(≤4, n−1)`; a higher `last_rpc` at n−1 cannot exist once H is majority-accepted at 4. Say so.
- **Which terms feed `advance_term`** (L482 vs L325/L360): does a bare `have`/`heartbeat.have_rpc`/`nack.have_rpc` (no verified proof) bump, or only an installed proof? Does a `commit` that is then ignored (L323 stale/different hash) still bump? Both safe under CFT; they differ on when a leader steps down. Pick one — recommended: any message-carried term bumps.
- **Stale-term check scope** (L489): stated for `append` only; an engineer generalizing to `commit`/`have` drops a valid old-leader proof (H2 installs a term-2 proof while leader of 4). Say "applies to `append`, `request_vote`, leader adoption; proof content is processed regardless of sender term."
- **Catch-up trigger role** (L527 "the follower MUST", L478): candidates too (L484 presumes a candidate's tail can move); add `append.prev_n > local head` as a trigger (changelog L13 says `have`/`prev`; L527 lists only `have`).
- **`nack` handling** (L489 "leader runs the Win catch-up path"): enumerate `have_n > prev_n` (leader fetches), `have_n < prev_n` (leader does nothing; follower catches up from heartbeat), `have_n == prev_n, hash ≠` (protocol violation, log).
- **Self-removal** (L425): majority is `majority(len(next_roster))` excluding self; the 3 s give-up is safe only if the removed node keeps gossiping `have` and serving `get_scenes` as an observer indefinitely — say so.
- **`vote.last_*`** (L465): informational — no rule consumes it (L467); delete the "receivers may ignore a vote whose `last_*` does not match" sentence (tails legitimately change within a term via catch-up; no safety role).
- **Startup sick vs absent** (L402 vs L858/L874): JSON-parse failure → absent; parses but fails hash/sig/apply → sick; `pending.n > head.n+1` after an absent file → unlink. Three readings today.
- **Stored-proof policy** (L517 idempotent no-op vs L389): keep the proof with the higher `rpc_term`; otherwise a node's tail can recede 4→2 on receiving an old proof (safe, but an engineer asserting monotone `last_rpc` is wrong).
- **Different-hash pending on `staged` acceptance** (L390 vs L517): delete old pending at `staged` too, not only at materialized commit.
- **Probe mechanics** (L469, L525): no request form and no wait bound; state "every node sends `have` to every connected peer every 500 ms; probe = one round or a majority of `have`s; `nack` (L473) corrects the rest."
- **§20** L853 "in that term" → "`rpc_term`"; L856 "Same hash at that scene.term" → commit happens at a later `rpc_term` with `scene.term` unchanged (Example H). L855 fine.
- `append` (L488) omits `room`; §16 L739 has it — follow §16.

## Review-5 status
E1 closed (L478/L525/L527/L533, test L892) — residual role/`prev_n` wording above. E2 closed (L425, L893) — residual observer-serving sentence. E3 closed (L402, L894). All review-5 minors present: `leader_id` not persisted (L352), `vote.last_*` defined (L465), `nack` (L743), `have {rpc_term}` (L734), `commit {room, scene}` (L741), torn-file-as-absent (L402). §16 matches §11 field-for-field (checked: `have`, `heartbeat`, `nack`, `commit`, `cert`, `vote`, `request_vote`).

## Verdict
§11 v1.5 holds one-hash-per-`n` under crash faults. The term floor plus vote-quorum intersection closes the Codex fork; the safety argument goes through on stated rules with no hidden assumptions. No blockers. One state-machine edge (Win abort on demotion) would produce observably different leaders; the rest are wording. Example I should be reworded to name the clause that actually stops S.

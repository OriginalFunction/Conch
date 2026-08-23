# Review 4: agent-room design spec v1.3 — §11 + Example H only

Target: `2026-08-23-agent-room-design.md` v1.3 (856 lines). Scope per author: re-prove §11 and Example H — (Q1) can B commit H without a second log slot, (Q2) can a zombie term-2 leader mint a second hash. Two independent provers (adversary; implementer) plus my own pass; findings merged, every quote re-verified. Prior: `-review-3.md` (v1.2).

## Answers

**Q1 — Yes.** Example H (L805) executes in one slot. B wins term 3 on tail `(2,n,H)` (E at `(≤2,n-1)` is refused, L377); carry-forward (L444) re-appends the exact hashed scene (`scene.term` 2, `scene.leader` A unchanged); C, D fire lifecycle row L358 — update `accepted_rpc_term/leader`, fsync a **new** cert bound to `(rpc 3, leader B)`, send it; B self-cert (L434) + C + D ≥ 3 at one rpc_term → `commit_proof{3, B, certs}`; direct commit (L448); push (L450). No n+1, no H′. A returns holding a valid term-2 proof for the same hash; both proofs are valid (L201).

**Q2 — No.** L424 (stale `rpc_term` rejected; certify only to this term's leader), L432 (leader counts only certs naming its own `rpc_term`/id), L446 (never a different hash while pending/head is H@n), L360 (pending replaced only by a *current*-term append), L363 (committed wins). A zombie's pending is H; late term-2 certs can only commit H (legal, same hash); H′@n+1 on its side cannot reach majority and loses to the committed value on heal. Both provers also re-derived the completeness argument (L381): a winner never carries a hash a majority accepted differently, including the fresh-mint case (a no-pending winner minting at head+1 needs majority votes ≤ its committed-head tail; an H-acceptor's tail is ≥ `(s,n,H)` and can't sink below it).

**Invariant 1 proof from the text** — holds. Steps: one leader per term (L387/L407/L409); one hash per (term, n) (L357/L434/L446/L360/L364/L429/L424); commit = majority one-term certs (L220/L315); every later leader has `last_n ≥ n` with H (L377 + majority intersection). Two steps rest on things the text does not argue: (i) quorum intersection across a view-change relies on single-change adjacency (L260) plus "n+1 only after n committed" (L366/L424); (ii) a node's tail can *drop* to a lower `commit_proof.rpc_term` (L374, L201) — it stays ≥ the first-majority-accept term only because a proof below that term is impossible, which is never stated. Neither breaks safety; both should be one sentence in §11.2.

---

## Blocker

### D1. Cert signature preimage is stated two incompatible ways — and one of them un-binds certs from the term.
- L153: "Sign the raw 32-byte SHA-256 digest … **For a scene cert, that digest is the scene hash above.**"
- L315 / L432: each cert sig "verifies over the 32-byte digest of JCS `{room, n, hash, rpc_term, leader, node}`".
Under the L153 reading a cert signs only the scene hash; `rpc_term`/`leader` are then unsigned wire fields, a cert for H made under (2, A) is indistinguishable from one under (3, B), and the term-binding that closed review-3's C2 (zombie counting later-term certs) exists only by convention. Two implementations also won't verify each other's certs. Genesis: L186 has the room key sign the raw scene hash, L315 admits the `room` entry into the proof and requires the payload preimage "for each sig"; the genesis proof's `rpc_term`/`leader` are unstated.
Fix: L153 → "for a cert, digest = SHA-256(JCS of the §11.4 payload); for the genesis `room` entry only, digest = scene hash, and it does not count toward majority"; step 9: "genesis proof is `{rpc_term:1, leader:creator}`".

---

## Major — liveness (safe, but wedges a majority-alive roster)

### D2. No catch-up trigger; `commit` carries no scene/room; a behind leader deadlocks.
- L424 "Reject append if they do not match local committed head" (silent, no nack); L450 `commit { n, hash, rpc_term, leader, certs }` (L668: no `room`, no scene) is "the only follower commit path besides catch-up `get_scenes`"; L454–460 define `get_scenes` but never say *when* a node fetches (L460 is only the negative); L413 heartbeat carries `{n, hash}` only.
Three permanent wedges, all found by both provers: (i) A commits H@n, pushes to C,D only, dies; B (pending `(2,n,H)`) wins — C,D grant on equal tails; B re-appends with `prev_n=n-1`; C,D reject (committed head is n); B has B+E = 2 < 3 forever, L446 forbids anything else at n, B's heartbeats keep C,D's timers reset so no re-election. (ii) Follower that missed the `append` receives a scene-less `commit` and can never apply it; leader retransmits "until their `have` matches" forever. (iii) Node with a different-hash pending receives `commit` for that n — L363 presumes an "incoming committed scene" no message delivers.
Fix: `commit` carries `room` and the scene (i.e. it is `scene {room, scene, commit_proof}`); on any `have`/`heartbeat`/`commit`/`append.prev_n` naming a committed `n > head`, or `n == pending.n` with a different hash, the node `get_scenes` from the sender and `apply(commit)` first; a follower rejecting an `append` whose `prev_n < own head` replies with its `scene{…}` for those heights (leader then fires L362 and clears pending).

### D3. L377 hash clause is unconditional and stalls a fresher candidate.
- L377 "If `last_n` equal, `last_hash` must match or refuse."
Roster {A,B,C}: A pending `(2,n,H)` (nobody else got it); B wins 3, no pending, mints X@n, C accepts, B+C commit X, B dies. C campaigns 4 at `(3,n,X)`: fresher, but `last_n` equal and X≠H → A refuses; A can never win (2<3). Majority {A,C} alive, no leader ever. The clause buys nothing: under L446 + one leader per term, equal `(last_rpc, last_n)` already implies equal hash.
Fix: restrict to "if `last_rpc` and `last_n` are both equal" (sanity check), and pair with D2 so A catches up to X.

### D4. A removed node can still campaign and win, then can never commit.
- L385 "Electorate = committed roster. Observers do not vote or campaign."; L407 grant rules never check candidate ∈ voter's committed roster; L409 counts voters in the *candidate's* roster.
Node d whose removal committed (or is pending at d) campaigns; b,c with equal tails grant; d wins, every scene it mints fails §10 step 4 (`leader ∉ roster`) while its heartbeats reset timers. Same if a leader commits its own removal.
Fix: voter refuses a candidate not in the voter's committed roster; a node not in its own committed roster never campaigns and steps down on committing such a view-change.

### D5. Re-running `precert` on a carried-forward grant can make H uncommittable forever.
- §11.4 step 1 "`apply(committed_state, scene, precert)`" runs before step 2 "same hash"; L313 precert requires `intent_id` be "the min `(ts, id)` among that follower's unconsumed uncancelled unexpired intents"; §12.3 lets a new intent supersede/cancel the old id.
Between A's append and B's re-append the holder re-raises (I1 cancelled) or an earlier-`ts` intent arrives by gossip; B,C,D all refuse H on re-append; L446 forbids any other hash at n. Root is §12.3's non-monotone precert, but the wedge lives in §11.
Fix: when `scene.hash == pending.hash`, skip step 1 and go straight to L358/L359 (bytes were already precerted); full precert only for empty or different pending. (Or make the §12.3 checks monotone.)

### D6. `request_vote` signing is an unresolved author note.
- L387 "send signed `request_vote`"; L405 "(`voter` omitted, extra key `typ` is not hashed if we keep hashed objects strict: put `typ` outside or include it consistently)"; L664 `request_vote` has no `sig`; whether `vote.last_*` are the voter's or the echoed candidate's tail is unstated.
Fix: add `sig`; preimage = SHA-256(JCS `{room, rpc_term, candidate, last_n, last_hash, last_rpc}`); say once in §16 that `typ` is a frame key excluded from every preimage; `vote.last_*` echo the candidate's.

---

## Minor
- L439 "send stored or new current-term cert" vs L358 "fsync new own cert": stored is legal only in the L359 row (same `rpc_term`, hence same leader). Reword.
- L360 permits replacing pending on a *same*-term different-hash append (only a misbehaving leader sends one); L779 "pending.json prevents a second hash in that term" is therefore not what the rules say; L781 "Same hash at that scene.term" is v1.2 wording. Add "and `rpc_term > accepted_rpc_term`" to L360; reword L779/L781.
- Restart/`leader_id`: L342 `leader_id?` in consensus.json, L364 "retransmit cert to `leader_id` if set", L411/L415 set it without fsync, L639 omits it. Resume-as-leader vs follower — both safe (pending + own cert fsynced before append), pick one.
- A `commit`/`scene` whose proof verifies must be applied regardless of `rpc_term` vs `current_term` (L424's "stale" is about `append`); say so, or an implementer drops A's late term-2 proof.
- Two valid proofs for one hash (L201): say "keep the higher `rpc_term`"; tail effect is eligibility only.
- L374 tail needs the head's proof `rpc_term`, which lives only in `scenes/<n>-<hash>.json`; L344 `head {n, hash, scene_term}` doesn't carry it. Say where it's read from.
- `have.term` (L661): `scene.term` or proof rpc? L450 "until their `have` matches" — on `(n, hash)` only, or B retransmits to A forever.
- Retransmit cadence (L357/L434/L450): "with each heartbeat (500 ms)". No step-down on lost quorum: fine, but L801 "that leader resumes if `rpc_term` is still highest" is practically unreachable (other side campaigns every timeout).
- §15 stale: L639 "term, voted_for", L640 "uncommitted cert" vs L343's six fields, L643 not `{scene, commit_proof}`; L214 `certs` array vs proof object; L663 `scene` message has no role after L450.
- L315/L220 never say `commit_proof.leader`/`rpc_term` may differ from `scene.leader`/`scene.term` — an implementer adding the natural equality check breaks Example H. One sentence.
- L350 replay raises `current_term` but doesn't clear `voted_for`/`leader_id` (L417 does); a proof delivered by `get_scenes` with higher `rpc_term` isn't treated like a message term.
- L805 "Followers retransmit their **signed** certs … so B can build the proof even if A's cert bag died" reads as reuse of the (2,A) certs; L358/L432 require new (3,B) certs. Say "re-sign for (3,B)".
- "Derived roster" (L134/L220/L315): for a view-change it is the roster of state n−1 (= envelope roster, L309), consistent with L385. Say so once; "derived" invites `next_roster`.

## Review-3 status
C1 (barrier) closed — L366/L448. C2 (term-less certs) closed by L424/L429–L432 **conditional on D1**. C3 closed — L260/L314. C4 closed — no noop. C5 half — push mandated (L450) but the message can't carry a follower to commit (D2). C6 closed — no `match` (L333/L439). §11 majors: catch-up at pending.n has a row (L363) but needs bytes (D2); `prev_*` has a target (L424) but no nack (D2); retransmit exists, no cadence; L377 two readings (D3); vote counting closed (L409); candidate∈roster still open (D4); restart/leader_id open.

## Verdict
§11 v1.3 is **safe**: the Paxos-style core (L358/L361/L377/L424/L432/L444/L446/L448) closes the v1.2 Figure-8 hole, Example H commits in one slot, and a zombie cannot mint a second hash. It is **not live**: D2 (no catch-up trigger, scene-less `commit`), D3 (unconditional hash clause), D4 (removed-node candidacy), and D5 (precert re-run on carry-forward) each wedge a majority-alive roster under the rules as written. D1 must be fixed before C2 can be said to be closed by cryptography rather than convention. All are pins, not design.

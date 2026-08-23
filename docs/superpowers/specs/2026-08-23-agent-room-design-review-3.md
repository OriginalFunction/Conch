# Review 3: agent-room design spec v1.2 — §11 and Example H first, then §10/§12

Target: `2026-08-23-agent-room-design.md` v1.2 (823 lines). Three independent reviewers (§11 safety proof + Ex F/G/H; §10/§11 implementer; §12 floor + forwarding), each reading the whole file; findings merged, deduped, every quote re-verified. Goals and §24 untouched. Prior: `-review.md` (v1), `-review-2.md` (v1.1).

## Is v1.1's R1 closed?
**The specific trace — yes.** Tail-based freshness (L364–370) makes every H-holder refuse a candidate without H; carry-forward of exact bytes (L412) and "never a different hash while log has an entry at n" (L414) force the winner to re-append H; replace-pending (L355) only fires on a current-term append from that winner. The late-cert variant is also closed (C with pending H refuses D; D's voters reject A's stale appends, L400).

**Invariant 1 is still not provable from the text**, for three reasons below (C1–C3). Fix C1 the obvious way without C2 and you get two heads again.

---

## Blockers — §11 (found independently by 2–3 of 3 reviewers)

### C1. Indirect commit (Figure 8) cannot execute: one `pending` slot, and `precert` runs against the committed head.
- L341 `pending.json { n, term, hash, scene } # uncommitted last log entry`; L359 "A single pending slot is enough: at most one uncommitted height (`head+1`)."
- L418 "proposes a **current-term** scene at `n+1` … When `n+1` directly commits, commit every prefix entry through `n+1`."
- L406 follower accept: "`apply(state, scene, precert)` where state is committed head"; L308 step 2 "`n == state.head_n + 1` and `parent == state.head_hash`".
- L352 leader self-propose "fsync pending including own cert" — overwrites H with noop; after that the leader "has no entry at n" and L414 no longer protects H.

Consequence: after *any* leader change with a pending entry, nothing at `n` can ever commit as written — Ex. H's "B proposes n+1 … that commit commits H", Ex. F's heal, and a singleton that crashes between L402's fsync and commit and restarts into a new term all wedge. An implementer who improvises will most likely direct-commit the carried-forward entry on majority matches — which is precisely the Figure 8 two-heads trace (S1 commits H(term 2) on {S1,S2,S3} in term 4, dies; S5 with pending X@n(term 3) beats (n,H,2) tails under L368, carries X, commits it).

### C2. `cert` is term-less with no stated destination; a zombie old-term leader can count certs made under a later term's carry-forward and commit; L216 blesses it.
- L637 `cert | room, n, hash, node, sig` (no `rpc_term`; `match` L638 has one); L351/L407 "send `cert`" with no recipient; L416 "majority of roster have that hash in log (certs + leader self-cert)"; L216 "A scene is committed when apply() accepts it, including majority certs"; no step-down-on-lost-quorum; a stale `append`/`heartbeat` is "rejected" (L400) with nothing sent back.
- Trace (S1–S5): term 2 S1 proposes H, S2 certs; S1 cut off from S3–S5 only (never sees a higher term). S5 wins term 3 with {S3,S4,S5}, mints X@n (legal, L412), crashes before append lands. S2 wins term 4 with {S2,S3,S4} on tail (n,H,2), carries H to S3,S4 → they `cert{n,H}`. If certs are gossiped (nothing forbids it) S1 now holds {S1,S2,S3} certs, `scene.term 2 == current_term 2` → L416 commits H; by L216 it *is* committed and leechers accept it. S2 dies before the barrier. S5 returns, campaigns term 5 with (n,X,3); S3,S4 at (n,H,2) grant (3>2, L368); S5 carries X; S3,S4 replace H (L355/L409); once C1 is fixed X commits. S1: H. S3–S5: X.

### C3. Multi-node `view-change` breaks quorum intersection → two leaders in one term, each with a "majority".
- L374 "Electorate = committed roster"; L256 `add`/`remove` "arrays of node ids (may be empty)"; L386 majority of roster; L216/L315 certs counted against the *derived* roster.
- R1={a,b,c}, view-change adds {d,e,f}; a commits it (certs a,b) while b still has it pending, c has nothing. Partition {a,d,e,f}|{b,c}. b wins term t with {b,c} (majority of *its* committed R1). a, leader of t−1 with R2, proposes X@n+1, commits with {a,d,e,f} (4/6). Heal: d,e see b's higher `rpc_term`, adopt b, replace X (L355); b's n+1 commits with {b,c,d,e}. a has X; the rest another hash. No one equivocated in the L742 sense. Even |add|=1,|remove|=1 is unsafe ({a,d} vs {b,e}).
- Fix: `|add| + |remove| == 1` per view-change (Raft single-server change), or joint consensus.

### C4. The `noop` barrier is the only legal follower of an uncommitted grant, but (a) `apply` step 6 forbids it while a grant is live and (b) it is optional.
- L312 step 6: "`closes_grant` omitted iff `state.live_grant` is absent, except genesis." noop (L258 "Required: `type` only"; unknown keys reject L149) can carry no `closes_grant` → rejected whenever the carried-forward H is a grant — Ex. H's own case. §12.5 L531 "any | noop | unchanged floor" contradicts step 6.
- L418 lists noop as one option among "the next real body"; L420 "Allowed only as that commit barrier." If H is a grant: no speech (grant uncommitted → no OPEN take, L819), no second grant (L529), nothing requires noop → H pending forever.
- Fix: step 6 exempts `noop`; on win with a carried-forward entry whose `scene.term < current_term` the leader MUST immediately propose `noop@n+1` (Raft start-of-term no-op) unless a real body is already queued.

### C5. No defined path by which a *follower* commits its pending; `scene` broadcast is never mandated.
- L353 "`commit` of that `n` → write scenes/, advance head, delete pending" has no trigger; L416 commit is leader-only ("`have` updates"); L388 heartbeat carries `{n, hash}`, no certs; L633 `scene` "committed scene" — only Ex. H (L775) mentions gossiping it; L315 commit mode needs majority certs.
- Reading A: leader pushes `scene` to all on commit. B: follower sees `have`/heartbeat n>head, issues `get_scenes`. C: certs are gossiped to all and everyone commits on local majority (which is what makes C2 possible). Text picks none; under A/B the follower also cannot accept `append n+1` until it has committed n, and no ordering is stated.

### C6. `match` is not a signature → the committed scene file can lack majority certs; Ex. H produces this.
- L407 "send `cert` if not yet sent for this hash, else `match { n, hash, rpc_term }`"; L638 `match` no `sig`; L422 "Collect certs/matches … Store them in the scene file. Catch-up commit mode verifies majority as in §10"; L315 `count(roster certs) >= majority`.
- In Ex. H, C and D already sent certs to dead A; on B's re-append they send `match`. B's scene file holds B's cert only; every later leecher's step 9 fails. Also: nothing persists collected certs before commit, so a leader crash loses them.
- Fix: drop `match` (or give it `sig`); `cert` is idempotent and always re-sent to the appending leader; leader persists collected certs in `pending.scene.certs`.

---

## Recommended resolution for C1/C2/C4/C5/C6 (one design decision)

Two coherent ways out. Pick one; do not mix.

**Option A — stay Raft-faithful.** `pending` becomes an ordered uncommitted suffix `head+1..head+k` (k ≤ 2 suffices); precert entry k against the speculative state from precerting k−1; tail = last suffix entry; "replace" = truncate-at-n-then-append (Raft log match via `prev_n/prev_hash`, with a mismatch reply and `get_scenes`); `cert` carries the `rpc_term` of the append it answers and goes to that leader only; leader counts only `rpc_term == current_term`; noop mandatory and step-6-exempt; `apply(commit)` of n+1 commits the suffix in order; leader pushes `scene` to all on commit; followers answer a stale append/heartbeat with their `current_term`.

**Option B — Paxos-style freshness, keep the single slot, delete Figure 8.** Tail freshness = `(accepted_rpc_term, n, hash)` where `accepted_rpc_term` is the `rpc_term` of the append under which this node last accepted the entry (for a leader's fresh entry, `current_term`); store it in `pending.json`; `cert` carries that `rpc_term` and goes to the leader; leader counts only certs with `rpc_term == current_term`; winner carries forward its own pending (it is guaranteed freshest among the majority); **direct commit is then always safe**, so L418/L420/noop go away and C1, C4, C6-via-match disappear. Re-check: Figure 8 shape — S1 carries H under rpc 4 to S2,S3 (acc=4), commits on current-term certs; S5 (3,X) can no longer beat (4,H) holders. Zombie — S1 counts only rpc-2 certs. C5 still needs the `scene` push; C3 still needs single-node view-change.

Option B is less text and fewer states. Either way, C3 and C5 are independent and must be fixed too.

---

## Major — §11 mechanics
- **Catch-up commit at `pending.n` with a different hash leaves pending in place** (lifecycle L349–357 has no row; L430). Node's tail becomes (n,H′) while head is (n,H); it refuses candidates that are ahead and, if elected, appends H′ over its own committed H. Fix: "commit from any source of any scene at `n ≥ pending.n` → delete pending."
- **`prev_n`/`prev_hash` (L400, L636) have no comparison target and no mismatch rule; no nack in §16**; "step down if the leader is wrong" (L356) is unactionable. Fix: compare to follower's tail; on mismatch reply `{rpc_term, head_n, head_hash}` and `get_scenes`.
- **No retransmission anywhere**: L407 "send cert if not yet sent" forbids resend; leader never re-appends; heartbeat carries no append → one lost frame wedges the term with no election. Fix: leader re-sends `append` for uncommitted entries with every heartbeat; follower re-certs each time.
- **L368 hash clause two readings** ("If `last_n` equal, `last_hash` must match … (divergent tails at same index/term)"): applies whenever `last_n` equal (refuses a fresher higher-term candidate) vs only when `last_term` also equal. Fix: state the latter.
- **Vote counting** (L386, L635): count only `vote.rpc_term == current_term`; refusals carry the voter's term and are always sent; candidate handling of `grant=false` unstated.
- **Behind follower has no catch-up trigger** (L388, L433 state only the negative). Fix: on `have`/heartbeat n>head, `get_scenes(head+1..n)`.
- Minor: `leader_id` in fsynced `consensus.json` (L340) vs §15 comment (L609) — restart as leader or follower? `pending.term` redundant with `scene.term`; `staged` scenes' storage/replay (§15 has only `scenes/`, replay would run `commit` without blobs); side-effect done-set has no file; candidate timer randomization/reset on vote; candidate ∉ roster still not a voter rule (L378–382); Ex. G "same-term different hash is refused" and L749 are not follower rules (L355 permits; only L414 + one-leader-per-term prevents); Ex. G "does not win that election" overstates (only loses this node's vote); Ex. F "resumes if no higher rpc_term" is practically unreachable (cut-off side campaigns every [T,2T]); `history` omitting noop makes agent `n` non-contiguous — say so.

## Major — §10
- Step 1 (L307) still cites §8.1/§9.1 only, not §9.2 body schemas; allowlist absent from step 8 (L314); `reason` untied to `floor_mode` (review-2 R11 open).
- `precert` return value undefined (L292 returns ChainState, L299 "advances: no") — needed for Option A's speculative state; leader never said to precert its own proposal.

## §12 floor (review-2 R3/R12/R15/R18 closed; R14/R16-partial/R17-partial open)
- **Freeze step 5 (L456)** "no `close_take` within FREEZE_WAIT **and the holder is unreachable**" — undefined conjunct, no `freeze` retransmit: reachable-but-silent holder wedges the floor forever (stick mode has no yank), or 5 s alone drops a 5.1 s reply's text. Fix: retransmit `freeze` every 1 s; empty-close at FREEZE_WAIT regardless; late `close_take` discarded.
- **Holder on `freeze` while CLOSING / after restart / for unknown or CLOSED grant** (L454 "if OPEN", L457 resend only "on leader change") — two implementers, one silent. Fix: reply idempotently on every `freeze`, restart, and leader change until `closes_grant == grant_hash` commits.
- **OPEN buffer and `request_id` table not on disk** (L442/446/459/461) — holder restart silently drops `ok`'d speaks, then freeze commits empty text under the agent's name. Fix: fsync per accepted speak.
- **Example A (L761) still contradicts L475**: after the grant commits (I1 consumed) Codex's `wait-for-floor` has no unconsumed intent → sends I2 with earlier `ts` → Codex re-granted before Claude. Fix: send `wait` only if no unconsumed intent AND live grant is not to this mouth.
- **Supersede/queue order not chain-derivable yet called "deterministic and global" (L495)** vs step 7 (L313) listing only bytes/sig/to/expiry. Follower with both I1,I2 refuses a grant to I1; follower with only I1 certs. Fix: "order and supersession are proposer-only; step 7 is exhaustive," or track last-granted `ts` per mouth in ChainState.
- **No `get_intent`; `append` doesn't carry the intent** (L495, L636) — follower that missed gossip refuses forever. Refresh copies "same id, new exp" (L491) vs "duplicate id is the same intent" (L493): which copy wins?
- **`breakout_req` (L645) carries no child ticket/genesis** though §14 mints the key on the holder node and leader ≠ holder in Ex. D.
- Minor: `yield` on a follower with unknown leader → `unavailable` (L514) vs freeze-locally (L454); `*_req`/`close_take`/`freeze` carry no sender binding (say: accepted only from the authenticated peer == `from.node`/`to.node`; `freeze` carries `rpc_term, leader`); moderator `grant_req` with no intent → which code; timeout clock L452 (grant ts) vs L471 (commit) and "followers don't re-check" not stated; `speak` vs `valid` still unsaid; retried `request_id` after freeze (L443 vs L446); `*_req` client reply timing and membership re-forward duplication; blobs on a holder that dies after `close_take`; leader skips intents from non-roster nodes?; L536 "yank without buffer" stale; `leave --vacate`, lower-hand, `kind` still unused.

## Example traces
- **F** (L771): all sentences follow except "carries forward any majority-accepted uncommitted entry" (winner carries *its own* pending, majority or not, L412) and the subsequent commit needs C1/C4.
- **G** (L773): "same-term different hash refused" is not a follower rule; "does not win" overstates.
- **H** (L775): every sentence follows through "B re-appends exact H" (L351, L416, L368–370, L412); "B proposes n+1 … that commit commits H" ✗ (C1, C4 if H is a grant, C6 for the cert file); final sentence conditional on it.

## Verdict
v1.2 closes v1.1's R1 trace and the single-proposer/forwarding/preimage gaps. §11 is still not buildable: the barrier that closes Figure 8 cannot run with one pending slot (C1), certs are not term-bound (C2), multi-node view-change breaks quorum intersection (C3), noop is both illegal-when-needed and optional (C4), and followers have no commit path / signature accounting (C5, C6). Choose Option A or B above, add single-node view-change and a mandated `scene` push, and §11 becomes provable. §12's open items are wording plus three real forks (freeze step 5, Example A, supersede semantics).

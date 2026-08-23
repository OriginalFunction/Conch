# Conch

Agent floor-control swarm. The software is Conch. A **room** is still a conversation (ticket, ledger, floor). Who holds the conch may speak.

Design spec. 2026-08-23. v1.6.

This file is the spec. The conversation that produced it is not normative. If this document and a memory of the chat disagree, this document wins.

Reviewers: look for contradictions, missing algorithms, and any requirement that two honest implementers would build differently. Do not relitigate goals unless a goal is impossible. Suggested order: Problem, Invariants, Consensus (§11), apply() (§10), Floor (§12).

## Changelog

**v1.6.** Safety of Example I confirmed. Win **aborts** if `advance_term` demotes the leader mid-probe. `advance_term` runs on installed proofs and signed/roster election terms, not on bare `have_rpc`. Example I reworded (vote-quorum at 101, then higher `last_rpc`). Nack cases, probe bound, self-removal keeps serving `have`.

**v1.5.** `current_term` is raised on every accepted commit proof (live catch-up, not only restart). Campaign starts at `max(current_term, tail.last_rpc)+1`. Followers catch up from `have`/nack. Self-removal pushes `commit` before step-down. Stale `pending.n <= head.n` is unlinked.

**v1.4.** Cert preimage is the cert payload, not the scene hash. Durable commit order + scene scan on restart. Catch-up when `have`/`prev` shows a higher committed n (D2 deadlock). Same-term different hash is a protocol violation. Removed nodes cannot campaign. Carry-forward skips grant-intent precert. `request_vote` is signed. `commit` includes `room` and is idempotent.

**v1.3** (third external pass). v1.2 closed Example H's "E wins" fork but the Figure 8 barrier needed H@n and a scene at n+1 at once, while the log allowed only one pending slot. That is not implementable. v1.3 drops Raft indirect commit and `noop`. Commit is **only** a majority of **current `rpc_term` certs** for that hash (Paxos accept). A new leader re-appends the same hashed scene, collects a new current-term cert set, and commits H in place. Single pending slot stays. Certs are a sidecar proof, bound to `(rpc_term, leader)`, retransmitted with every accept. View-change changes exactly one roster member. Leader must push `commit`. No unsigned `match`.

**v1.2.** Tail-aware election, exact-hash carry-forward. Product §24 unchanged.

**v1.1.** One `apply()`. Floor reducer.

## 1. Problem

Coding agents (Claude, Codex, Grok, local LLMs) can split work internally, but that stays a black box inside one product. Sharing work across products is still a human with a markdown file.

Two failure modes:

1. **Handoff is slow.** The human is the message bus. Context dies in copy-paste.
2. **There is no floor.** If several agents append to the same file, they all speak at once. A file is not a mutex. Agents are trained to complete, not to listen.

What is missing is not a faster chat server. LLM tokens dominate latency. What is missing is a floor-control bus: who may speak, who is queued, who is only listening, and a history every participant can trust.

A second poison mode matters as much as collisions. If even one participant's history is wrong, the next take is generated from a lie. Once that take is accepted, the lie is frozen. Corrupt history poisons the future.

## 2. Product picture

A **room** is a torrent-shaped object for a conversation that is still being created. The swarm shares wrapped scenes, not rough takes.

You copy a small ticket (`.room` file, magnet, or URL). Any machine opens it and joins the swarm. Every node stores the whole film. New machines leech missing pieces, verify hashes, then seed.

The extra rule BitTorrent does not have: a piece is not in the torrent until the current roster's majority has certified it under the consensus rules below.

**Who this is for first.** One operator, several machines (home minis, a laptop, burst EC2, an optional public nano with a FQDN), several vendor agents plus local LLMs.

**Daily loop.** Four minis already joined a lobby. You take the stick, state a task, breakout. Listed minis auto-join a child room. You watch the ledger.

**Cold start.** An agent writes a ticket. You `conch join` it on each machine.

**Nano.** A cheap public node that stores a full replica and introduces peers. It does not vote and cannot be leader. You may terminate it. Seeders that already have the film and each other continue.

There is no master *machine*. Any staker may be elected leader of a consensus term. That office is not a special box. It is how one hash wins at each height.

## 3. Goals

- Any agent that can run a CLI or MCP joins from a ticket. No vendor SDK.
- Exactly one speaker at a time, enforced by wrap, not by prompting.
- Every node stores the entire verified conversation. CLI, MCP, and web UI can read it.
- Join and leave whenever. Catch-up is leeching. Going offline does not trim history.
- No master machine. Join any live peer on the ticket, same swarm.
- Public introducer is optional and killable.
- Durability over speed. Frozen floor beats two speakers.
- Human can participate or only watch.

## 4. Non-goals (v1)

- Public chain, crypto tokens, smart contracts, anchoring heads to Ethereum. A ticket capability secret is not a crypto token.
- DHT or discovery with zero reachable peer.
- Built-in Tailscale or any overlay. Overlay addresses are ordinary strings if you put them in the ticket.
- UDP/QUIC or SSH transports.
- Light clients, pruning, or serving unverified history to agents.
- Byzantine tolerance of lying stakers. v1 nodes are the operator's machines. Auth is keys plus a capability, not BFT.
- Making an LLM "be polite about turns."
- Team SaaS, accounts, mobile apps, search-as-product.

## 5. Glossary

| Term | Meaning |
|---|---|
| Ticket | Small JSON file, magnet, or URL. How you find a room. Not the history. |
| Conch | This software. Binaries `conchd` (node) and `conch` (CLI). |
| Node / `conchd` | Daemon on a machine. Swarm peer. Stores the ledger. |
| Agent | A mouth attached to a node: `claude`, `codex`, `human:ray`. Identified as `(agent, node)`. |
| Scene | One ledger record at height `n`. Content-addressed. |
| Term | Raft-style counter. Leader of a term is the only proposer. |
| Wrap / commit | Majority of the committed roster has certified the leader's scene at `n` for that term. |
| Grant | Wrapped scene that names who holds the stick. |
| Roster | Ordered list of staker node ids committed on chain. Jury and electorate. |
| Staker | Node on the roster. May vote, certify, be leader. |
| Observer | In the swarm, full replica, no vote, never leader. The nano is one. |
| Intent | Signed raise or wait request with a stable id. Not a scene. |
| Head | Last scene `apply()` accepted, blobs on disk. |
| Valid | Local chain from genesis to head passes `apply()`, blobs present. |
| Healthy | Valid, and a majority of the committed roster has advertised this same head recently (or roster size is 1). |
| Breakout | Child room spawned by a wrapped scene. |

`majority(n) = n/2 + 1` integer division. 1→1, 2→2, 3→2, 4→3, 5→3.

## 6. Architecture

Three programs, one object (the ticket).

| Program | Role |
|---|---|
| `conchd` | Node. Ledger, consensus, floor, stake, swarm, transports, static UI. |
| `conch` | CLI. First-class client. |
| `conch mcp` | stdio MCP. Same verbs as the CLI. |

An agent talks to a node (default localhost). The node is in the torrent. The LLM never speaks the swarm protocol.

```
 agent / UI / CLI / MCP
          |
    client protocol
          |
        conchd core
          |
    swarm messages
          |
   [ ws ] [ tcp ] [ http ]
          |
       other nodes
```

**Language.** Rust for `conchd` and `conch`. UI is a small web app bundled into `conchd`.

**Default data dir.** `~/.conch`. Override `--data-dir`.

**Default listen.** HTTP+WS on `0.0.0.0:7420`. TCP on `0.0.0.0:7421`. Loopback-only if `--localhost`.

**Why not libtorrent, Iroh, or Ethereum.** Churn and piece hashes are torrent-shaped. A growing movie with a mutex is not an immutable `.torrent`. A public L1 makes every grant a paid public transaction. The room chain is blockchain as data structure plus quorum. The wrap engine is Raft-shaped majority vote among the operator's nodes, not CometBFT and not Ethereum. That engine can still be swapped later without changing tickets or CLI.

## 7. Invariants

Testable. Violate one and the implementation is wrong.

1. **One head (crash-fault).** Among non-equivocating nodes, there are never two committed scenes at the same `n` with different hashes. A malicious binary that signs two ways is outside v1 (see §19).
2. **One stick.** At most one grant is live. A grant is live if it is committed and no later committed scene has `closes_grant` equal to its hash.
3. **No speak without grant.** `speak` is rejected unless the take is OPEN for that `(agent, node)`.
4. **Poison.** Agents are never served unverified or draft scenes as `history`. Signing and proposing require `valid`.
5. **Minority never wraps.** Fewer than `majority(len(roster))` roster certs cannot commit.
6. **Roster does not shrink by timeout.** Missing certs do not remove a voter from this scene's roster.
7. **Roster changes only on chain.** Jury is genesis roster plus committed view-changes.
8. **Full replica.** A node in the swarm stores every committed scene from genesis to its head, plus referenced blobs.
9. **Observers never certify, never lead, never hold the stick.** Floor holders' `to.node` must be on the roster.
10. **Same bytes.** Two valid nodes at the same head have identical scene hashes for `0..=head`.

## 8. Ticket

Rendezvous plus capability. Not the movie.

### 8.1 Encoding rules (all JSON in this spec)

Apply to tickets, scenes, intents, votes.

- UTF-8 JSON. RFC 8785 JCS before hashing.
- All hex is lowercase. SHA-256 is 64 hex chars. ed25519 public key / node id is 64 hex chars (32 bytes). ed25519 signature is 128 hex chars.
- Hashed objects: unknown keys are a hard error. Optional fields are **omitted**, never JSON `null`, except `parent` on genesis which is JSON `null`.
- Hash of a scene: SHA-256 of RFC 8785 JCS of the object with the `certs` **key deleted** (not `"certs": []`). Result is 32 raw bytes. Display/transport as 64 hex chars.
- **Signature preimage.** Sign the **raw 32-byte SHA-256 digest**, never hex ASCII, never raw JCS.
  - **Commit cert** (the term-binding that closes zombie-leader tricks): digest = SHA-256(JCS of `{room, n, hash, rpc_term, leader, node}` with no extra keys). Not the scene hash. `leader` here is the leader of this `rpc_term` (may differ from `scene.leader`, the original proposer).
  - **Room-key genesis signature:** digest = the scene hash (32 bytes). Node certs on genesis still use the commit-cert payload with `rpc_term=1`, `leader=creator`.
  - **vote, request_vote, intent, decl, leave:** digest = SHA-256(JCS of that object with `sig` deleted).
  Two implementations that agree on these objects must verify each other's signatures.
- `token` in a ticket is 32 raw bytes shown as 64 hex chars. `token_sha256` is SHA-256 over those 32 raw bytes, lowercase hex.

### 8.2 Ticket object

Required keys: `v`, `id`, `name`, `trackers`, `peers`, `stake`, `floor`, `genesis`.
Optional: `token`, `parent` (omit if not a breakout).

| Key | Type |
|---|---|
| `v` | integer, must be 1 |
| `id` | node-id hex, the room public key |
| `name` | string, 1–128 chars |
| `trackers` | array of endpoint strings |
| `peers` | array of endpoint strings, may be empty |
| `token` | 64 hex chars if present |
| `stake` | object: `agents` bool, `explicit` bool, `allowlist` array of node ids |
| `floor` | object: `mode` (`stick` or `moderator`), `timeout_secs` integer ≥ 1, optional `moderator` `{agent, node}` required if mode is `moderator` |
| `parent` | room id hex if breakout |
| `genesis` | SHA-256 hex of genesis scene without certs |

Ticket unknown keys: ignored on read (not hashed). `v` must be 1.

### 8.3 Magnet

```
conch:1:<id>?dn=<name>&g=<genesis hash>&tr=<url>&x.peer=<url>&token=<hex>
```

`g` is required (same pin as the JSON `genesis` field). `tr` and `x.peer` may repeat. Parser accepts magnet, filesystem path, or `http(s)` URL that returns the ticket JSON. One parser.

### 8.4 Genesis vs file

Genesis is scene `n=0`, `term=1`, `parent=null`, `roster=[creator_node]`, `leader=creator_node`. Body type `genesis`. Room secret key signs the raw 32-byte scene hash as `{ "node": "room", "sig": "..." }`. The string `room` is reserved; it is not a 64-hex node id.

Join: fetch genesis, verify room signature against `id`, verify ticket/`g` matches the hash. Refuse on mismatch.

Room secret key lives only on the creating node: `rooms/<id>/room.key`. Needed for genesis and human re-genesis. Not used to speak.

`GET /ticket/:id` requires `Authorization: Bearer <token>` when a token is set; otherwise 401. Response omits `token` (the client already has it). Unauthenticated fetch of a token-bearing ticket is forbidden.

## 9. Ledger

If a scene is not committed, it did not happen.

### 9.1 Scene envelope (hashed except `certs`)

Required: `v`, `room`, `n`, `term`, `parent`, `roster`, `leader`, `ts`, `body`.
The **immutable log entry** is the canonical scene with the `certs` key deleted. That is what is hashed and what "exact bytes" / carry-forward means. `certs` / `commit_proof` is a mutable sidecar: current-term signatures that prove commit. Two nodes may store different proof sets for the same hash; both are valid if each is a majority at a single `rpc_term`.

| Key | Type |
|---|---|
| `v` | 1 |
| `room` | room id hex |
| `n` | integer ≥ 0 |
| `term` | integer ≥ 1 |
| `parent` | SHA-256 hex, or `null` iff `n=0` |
| `roster` | array of node ids, sorted lexicographically, unique, length ≥ 1 |
| `leader` | node id of the proposer; must be in `roster` |
| `ts` | Unix seconds, integer, advisory |
| `body` | object with `type` string |
| `certs` | array of `{node, sig}` |

Do not put floor snapshots, raised queues, or agent names in the envelope. Derive floor from the chain (§12). `leader` is the proposing node, not the talking-stick holder.

A scene is **accepted** when a node fsyncs it into `pending.json` (or as leader self-accept). Accepted is not committed.

A scene is **committed** when `apply(..., commit)` accepts a **commit_proof**: majority of the derived roster, all certs the same `rpc_term`, each cert bound to the same `leader`, signatures valid. Envelope `roster` must equal the derived roster. Old-term accepts, unsigned matches, and mixed-term cert bags are not a proof.

### 9.2 Body types

`body.type` is one of: `genesis`, `grant`, `speech`, `breakout`, `membership`, `view-change`.

**genesis** (only `n=0`)

Required: `type`, `name`, `stake`, `floor`, `creator_node`.
Optional: `parent_room` (omit if none), `token_sha256` (omit if no token).

`floor.moderator` is `{agent, node}` if mode is `moderator`, otherwise omit.

Genesis also needs the room signature in `certs`. It is committed when the room sig verifies and the creator's node cert is present (`majority(1)=1`).

**grant**

Required: `type`, `to`, `reason`, `intent_id`.
`to` is `{agent, node}`. `reason` is `queue` or `moderator`. `intent_id` is 32-byte hex. `to.node` must be on the roster. `to.agent` 1–64 chars matching `[a-z0-9_.:-]+`.

**speech**

Required: `type`, `closes_grant`, `text`.
Optional: `blobs` (omit if none).
`closes_grant` is SHA-256 hex of the live grant. `text` is a string (may be empty). `blobs` is an array of `{name, sha256, bytes}` with `bytes` matching the blob length.

**breakout**

Required: `type`, `closes_grant`, `ticket`, `auto_join`.
`ticket` is a full child ticket object (may include `token`). `auto_join` is an array of node ids, subset of current roster.

**membership**

Required: `type`, `stake`, `floor`.
Optional: `closes_grant` (omit if floor vacant).

**view-change**

Required: `type`, `add`, `remove`, `next_roster`.
Optional: `closes_grant` (omit if vacant).
`add` and `remove` are arrays of node ids. **Exactly one membership delta:** `|add| + |remove| == 1` (one add or one remove, not both, not a batch). Joint consensus is out of v1. `next_roster` must equal sort(unique((current roster minus remove) plus add)). `next_roster` length ≥ 1. Multiple joins are sequential view-changes.

### 9.3 Blobs

SHA-256 of raw bytes. Max 32 MiB each. Stored under `blobs/<sha256>`.

A node **must not certify** a scene that lists blobs until each blob is on disk, `bytes` matches file length, and SHA-256 matches. The leader sends blobs with the proposal (`blob_meta` + raw frame) or followers `get_blob` from the leader before certifying.

Commit of a blob-referencing scene implies a majority of the roster had the bytes. If the original speaker dies after commit, copies exist.

### 9.4 Chain state

`ChainState` after applying scenes `0..=n`:

```
head_n, head_hash, head_term
roster                 # node ids
stake                  # policy
floor_mode             # stick | moderator
moderator              # {agent, node} or absent
timeout_secs
live_grant             # {hash, to, term, n} or absent
consumed_intents       # set of intent ids closed by grants
```

Initial empty state before genesis: no head, no roster.

## 10. apply()

One pure transition on `ChainState`. Callers pass a **mode**. The reducer does not gossip or fsync.

```
apply(state, scene, mode) -> Result<ChainState, Error>
```

`mode` is one of:

| Mode | Certs | Intent bytes | Blobs | Advances `head` / `valid` |
|---|---|---|---|---|
| `precert` | skip majority (step 9) | required if grant | required | no |
| `commit` | required | not required | required | yes |
| `staged` | required | not required | may be missing | no; scene is unmaterialized |

`have`, agent `history`, and poison/`valid` use only `commit` results. `staged` is a disk cache until blobs arrive, then re-run `commit`. A node does not advertise a staged `n` in `have`.

Shared checks (all modes):

1. Envelope passes §8.1 and §9.1. Unknown hashed keys: reject.
2. `n == 0` iff state is empty. Else `n == state.head_n + 1` and `parent == state.head_hash`.
3. `scene.roster == state.roster` (for `n=0`, `scene.roster == [body.creator_node]`).
4. `scene.leader` is in `scene.roster`.
5. `scene.term >= 1`. If `n>0`, `scene.term >= state.head_term`.
6. `body.type` allowed by §12.5. `closes_grant`, if present, equals `state.live_grant.hash`. `closes_grant` omitted iff `state.live_grant` is absent, except genesis.
7. Grant: `intent_id` not in `consumed_intents`; `to.node` in roster. `precert` also: intent bytes present, sig verifies (raw 32-byte digest), `to` matches, not expired per §12.3 (`scene.ts < intent.exp`), and `intent_id` is the min `(ts, id)` among that follower's unconsumed uncancelled unexpired intents.
8. View-change: `|add| + |remove| == 1`; `next_roster` arithmetic exact; ids 64-hex.
9. `commit`/`staged` only: a `commit_proof { rpc_term, leader, certs: [{node, sig}] }`. All certs share that `rpc_term` and `leader`. That `leader` is the assembler of this proof and **may differ from `scene.leader`**. Unique `node`s, each in derived roster. Each **node** sig verifies over the commit-cert digest in §8.1. On genesis, the extra `{ "node": "room", "sig" }` is the **room-key** signature over the **scene hash**, not the cert payload; it does not count toward majority. Majority still needs the creator's node cert. Observers never appear. Mixed-term bags and unsigned matches are invalid. Stale `commit` for a different hash at an n we already committed: ignore (protocol violation). Same hash: idempotent.

After `apply(..., commit)` **or** `staged` acceptance of a proof, the caller MUST run §11.1 `advance_term(commit_proof.rpc_term)` before any campaign, vote, or cert.
10. Blobs: `precert`/`commit` require each blob on disk, length and SHA-256 match. `staged` may omit them.

On `commit` success, new state:

- head_* from this scene
- if view-change: roster = next_roster
- if membership: stake/floor_mode/moderator/timeout from body
- if grant: live_grant set, intent id consumed
- if `closes_grant` present: live_grant cleared
- genesis sets roster, stake, floor from body

Side effects that are not ChainState (breakout auto-join, unblocking wait) run on `commit` only, idempotent by scene hash.

**Genesis.** Creating node writes genesis, signs with room key and node key (raw digest), `commit`s, roster `[self]`, term 1, is leader of term 1 until it steps down. Not produced by `append`.

## 11. Consensus

Paxos-style accept + current-term certs. One proposer per `rpc_term`. Single uncommitted slot. No Figure 8 barrier, no `noop`, no unsigned `match`.

**Accepted vs committed.** A node may accept H@n (pending) without the value being committed. Commit requires a majority of **certs for this `rpc_term` and this `leader`**. A majority of old-term accepts is not a proof. Catch-up never treats "lots of unsigned matches" as commit.

**Immutable scene vs sidecar.** Carry-forward copies the hashed scene (certs key deleted). New certs are a new sidecar. The JSON file on disk after commit is `{ "scene": <hashed object>, "commit_proof": {...} }`.

### 11.1 Persistent state (fsync before the matching send)

```
consensus.json   { current_term, voted_for }   # do not persist leader_id
pending.json     { n, hash, scene, accepted_rpc_term, accepted_leader, cert }
head             { n, hash }                   # cache; election uses commit_proof.rpc_term
scenes/<n>-<hash>.json   { scene, commit_proof }
```

`cert` in pending is **this node's** cert for `(hash, accepted_rpc_term, accepted_leader)` so it can be resent.

**Term floor (normative, every path).** After restart replay, after `apply(commit)`, after `staged` proof acceptance, after installing a `commit` / `get_scenes` scene:

```
advance_term(proof_rpc):
  floor = max(pending.accepted_rpc_term if pending else 0,
              committed_head.commit_proof.rpc_term if head else 0,
              proof_rpc)
  if floor > current_term:
    current_term = floor
    voted_for omit
    leader_id omit   # memory only
    become follower
    fsync consensus.json
    # caller MUST abort any in-progress Win/append for a lower term
```

`proof_rpc` is taken only from a **verified** `commit_proof` after `apply(commit)`/`staged`, or from a **roster member's** signed `request_vote`/`vote` `rpc_term`, or from `append.rpc_term` / `heartbeat.rpc_term` on a connection whose `hello` node id is in the roster. **Bare `have.rpc_term` / `nack.have_rpc` do not call `advance_term`.** A liar advertising 10^9 on have cannot bump the floor. Installing their unverified number is forbidden; installing their verified proof is required.

Invariant: `current_term >= max(pending.accepted_rpc_term, committed_head.commit_proof.rpc_term)`. Live catch-up is not exempt. Campaigning, voting, and certifying while below that floor is a spec violation.

A valid `commit_proof` for a hash we do not yet have at that `n` is still installed even if `commit_proof.rpc_term < current_term` (old leader's proof). Higher current_term does not drop a legal proof.

This closes the term-100 / campaign-at-2 fork: nodes that applied P@n-1 with proof 100 have `current_term >= 100` before they can campaign.

**pending.json lifecycle**

| Event | Action |
|---|---|
| Accept append at `n=head+1` | fsync pending (scene + own cert), send `cert` to **that leader only** |
| Leader self-propose | fsync pending with own cert, then `append`. Retransmit `append` until certs or higher term |
| Same hash, new `rpc_term` (carry-forward) | update `accepted_rpc_term`/`accepted_leader`, fsync **new** own cert, send it. Do not keep the old-term cert as current |
| Same hash, same rpc_term | retransmit the stored cert (do not sign a second payload) |
| Different hash, n not committed | replace pending **only if `rpc_term > pending.accepted_rpc_term`** and `precert` passes; then new cert. Same `rpc_term` and different hash: **protocol violation**, refuse, do not certify (a leader must not ask one node to sign H and H′ in one ballot) |
| Commit of this n | see durable order below |
| Incoming committed scene at pending.n, same hash | commit, delete pending |
| Incoming committed scene at pending.n, different hash | committed wins; replace, `apply(commit)`, delete old pending |
| Restart | reload pending; `leader_id` is unknown until `append`/`heartbeat`; retransmit cert only after adopting a leader |

One slot: at most `head+1` uncommitted. Commit of n happens **in place** (new current-term certs for the same hash), never by appending n+1 first.

**Durable commit order (normative).** Crash between any two steps must not lose H and also lose the tail that protects it.

1. Write `scenes/<n>-<hash>.json` (`{scene, commit_proof}`). fsync the file and its directory.
2. Durably record that n is committed: fsync `head`, **or** (required on every startup anyway) treat `scenes/` as the source of truth by scanning and `apply(commit)` from genesis through max n that verifies. `head` is a cache.
3. Unlink `pending.json`. fsync the directory.
4. Only then send `commit` to peers.

Startup: scan `scenes/`. Unreadable, zero-length, or JSON-torn files are **absent** (skip), not a second history and not automatic `sick`. Replay `apply(commit)` from genesis through max n that verifies. Then: if `pending` exists and `pending.n <= head.n`, **unlink pending** (stale). Do not invent pending for a hash that is already a committed file. Then `advance_term` from the recovered head proof.

### 11.2 Tail (election freshness)

```
if pending:
  tail = (last_rpc = accepted_rpc_term, last_n, last_hash)
else:
  tail = (last_rpc = commit_proof.rpc_term of head, last_n = head.n, last_hash)
```

Candidate A is at least as up to date as voter B iff `A.last_rpc > B.last_rpc` OR (`A.last_rpc == B.last_rpc` AND `A.last_n > B.last_n`) OR (`A.last_rpc == B.last_rpc` AND `A.last_n == B.last_n` AND `A.last_hash == B.last_hash`). If `last_rpc` and `last_n` are both equal and hashes differ: refuse (divergent tails at the same ballot/index). If `last_rpc` differs, **do not** compare hashes; the higher `last_rpc` wins even when `last_n` matches (stale voter with old H@n still votes for a candidate whose committed head is X@n at a higher proof term).

Voters refuse a less-up-to-date candidate. E with only committed n-1 cannot beat B/C/D who accepted H@n. Winner is one of B/C/D and already has H.

A minority node that accepted X@n at a **higher** rpc_term than H could look more up to date. It cannot have become leader of that higher term without a majority of votes, which H-holders would refuse if their tail is newer than the candidate's then-tail. So X cannot be majority-accepted at a higher term while H was majority-accepted at a lower term. CFT plus this comparator is the completeness argument.

**Quorum intersection across view-change.** Consecutive committed rosters differ by exactly one node (§9.2). Any majority of R and any majority of the next roster share a node. Completeness survives single-node add/remove.

**Tail `last_rpc` never recedes below the first majority-accept term of that hash.** For a committed head it is `commit_proof.rpc_term`, which is ≥ the `rpc_term` of the cert set that first chose the value.

### 11.3 Election

Electorate = committed roster. Observers do not vote or campaign. **A node id not in the current committed roster MUST NOT campaign, vote, or be counted.** Ignore `request_vote` / `vote` from non-members. After a view-change **removes** this node: finish durable commit order and `commit` push to the **new** roster. Wait until `have` from `majority(len(next_roster))` (the removed node is not in that count). Until then this node **keeps serving** `have` / `get_scenes` / `commit` so the remaining members can catch up. **Then** step down, `leader_id` omit, do not campaign, treat as observe. Bound the wait at 3s then step down anyway (liveness); those who received the file proceed.

**Election timeout.** Default T=3s, randomized `[T, 2T]` per campaign. No current-term `append`/`heartbeat`/`commit` in that window → campaign (follower or candidate). Candidate who does not win by timeout: campaign again.

**Campaign.** Timeout **and we are in the roster**:

```
current_term = max(current_term, tail.last_rpc) + 1
voted_for = self
leader_id omit
fsync consensus.json
send signed request_vote
```

Count a self-vote. Never campaign at a term ≤ `tail.last_rpc`.

**Vote message (signed)**

```
{
  "room": "<id>",
  "rpc_term": 4,
  "voter": "<node id>",
  "candidate": "<node id>",
  "last_n": 7,
  "last_hash": "<hex>",
  "last_rpc": 3,
  "grant": true,
  "sig": "<ed25519 over raw SHA-256 of JCS without sig>"
}
```

**request_vote (signed by the candidate)**

```
{ "room", "rpc_term", "candidate", "last_n", "last_hash", "last_rpc", "sig" }
```

Same digest rule as other signed objects (`sig` deleted before JCS). Receivers verify `candidate` is the peer's node id. `typ` lives on the frame, not inside the hashed object. `vote` hashed keys are exactly the vote fields except `sig`. Only signed `vote` messages with `grant=true` are counted. `request_vote` is not a vote.

**Grant** when: (1) `rpc_term >= current_term` (if greater: `advance_term` on this signed vote/request_vote, continue), (2) `voted_for` omitted or equals candidate, (3) candidate tail ≥ ours. Then fsync `voted_for`, send `vote` with `grant=true` and `last_*` equal to this voter's tail at this instant. `last_*` is informational. Counting a vote requires only: sig, voter in roster, `grant=true`, `rpc_term` equals the campaign, `candidate` is us, unique voter.

**Count.** Unique `voter` in roster, `grant=true`, `rpc_term` equals the campaign term, `candidate` is us, sig verifies. Majority wins.

**Win.** Let `won_term = current_term`. `leader_id = self`. Heartbeat immediately. Probe `have` from connected roster members for **at most 500ms** or until a majority of the roster has replied, whichever first. Then:

- If any peer's `have.n >` our committed head: `get_scenes` and `apply(commit)` until caught up. That `apply` runs `advance_term` on the **installed proof**, not on `have_rpc` alone.
- If our pending n equals a peer's committed `have.n` and **same hash**: fetch `{scene, commit_proof}`, `apply(commit)`, delete pending. Do **not** re-append.
- If pending n equals a peer's committed n and **different hash**: committed wins (`apply(commit)`), delete pending.
- Only if `current_term == won_term` and we are still leader and pending is still uncommitted: carry-forward append (§11.4).

**Abort.** If during the probe `advance_term` raises `current_term` above `won_term`, or we are no longer leader: **stop**. Do not `append`. Do not self-cert at the new term. We lost the office. We may later campaign at `max(current_term, tail.last_rpc)+1`.

This is the A-commits-to-C,D-then-dies case: B wins, sees C/D `have` at n with H, installs their proof, does not append with `prev_n=n-1`.

**Heartbeat.** Every 500ms (`T/6`, T=3s). `heartbeat` includes committed `have` (`n`, `hash`, `have_rpc`). `append` also resets follower timers. Followers with `have.n >` local head start §11.5.

**Adopt leader.** `append` or `heartbeat` with `rpc_term == current_term`: set `leader_id` to that node (even if we voted for someone else). Reset election timer.

**Higher term.** Roster `append`/`heartbeat`/`request_vote`/`vote` with election `rpc_term > current_term`, or an **installed** `commit_proof.rpc_term` above `current_term`: `advance_term(...)`, then process. Not `have`/`nack` numbers.

**Retry.** Candidate timeout: campaign again using `max(current_term, tail.last_rpc)+1` (not a bare `+= 1` if tail moved).

### 11.4 Replicate, certify, commit

`append { rpc_term, leader, prev_n, prev_hash, scene }`
`prev_n`/`prev_hash` are the **committed** head. Reject append if they do not match local committed head. On reject, send `nack { room, have_n, have_hash, have_rpc }` (the follower's committed `have`). `have_rpc` does not bump the leader's `current_term` until a proof is installed. Leader handling:

1. `have_n >` leader committed n: `get_scenes` from that follower (leader is behind). Then Abort check.
2. `have_n == prev_n` and hash differs: protocol violation. Do not overwrite our committed head. Ignore this nack for catch-up.
3. `have_n < prev_n`: follower is behind. Send `commit` / `get_scenes` to them. They also run §11.5.

Reject stale election `rpc_term` on append (do not apply the scene). Do **not** discard a valid older `commit_proof` we already have or are installing. Certify only to `leader` in this `rpc_term`. A zombie leader of an old term is ignored (stale rpc_term) and cannot count new certs (certs name a different leader/term).

**Cert payload (signed)**

```
{ "room", "n", "hash", "rpc_term", "leader", "node" }
```

Sig = ed25519(raw SHA-256(JCS)). Leader counts certs with its `rpc_term`, its node id as `leader`, this `hash`, unique roster `node`.

**Leader self-cert.** Fsync pending + own cert, then append. Retransmit append until majority certs or higher term. Singleton: own cert is majority.

**Follower**

1. If `pending.hash == scene.hash`: **skip `precert`** (including grant intent min-queue). The value is already accepted. Issue a cert for the **new** `(rpc_term, leader)` if greater than `accepted_rpc_term`, else retransmit the stored cert. This is carry-forward: intent gossip must not make a previously accepted grant uncommittable.
2. Else `apply(committed_state, scene, precert)` (intents and blobs required for a **new** grant hash).
3. Empty pending: fsync, cert.
4. Different uncommitted hash: only if `rpc_term > pending.accepted_rpc_term`; replace, cert.
5. Blobs for new hashes: leader sends them or `get_blob`.

Same `rpc_term`, same hash: retransmit **stored** cert (do not sign a second payload). New `rpc_term`, same hash: sign a **new** cert payload (new `rpc_term`/`leader` in the preimage).

**Carry-forward.** On win, if pending: `append` the **same hashed scene** (not a new body, not rewritten `scene.term`). Collect **new** current-term certs. On majority: `apply(commit)` with this term's `commit_proof`, delete pending. If no pending: fresh scene at `head+1` with `scene.term = current_term`, `scene.leader = self`.

**Never** a different hash at `n` while this leader's pending/head is already H@n.

**Commit is always direct.** Majority current-term certs for H → H is committed. No n+1 barrier. No `noop`.

**Leader MUST push** `commit { room, n, hash, rpc_term, leader, certs, scene }` to all peers. `scene` is the hashed object (certs key deleted) so a peer that missed `append` can apply immediately. If a receiver already has the same hash committed: **idempotent success**, no-op. If it has pending same hash: `apply(commit)`, delete pending. If it lacks blobs: `staged` then fetch blobs. Retransmit `commit` every 500ms until that peer's `have` is `{n, hash}` or a higher term appears.

`get_scenes` remains valid: it returns `{scene, commit_proof}` for peers that want a range. Either path is enough.

**Retransmit cadence.** `append` every 500ms to peers that have not cert'd this `(n, hash, rpc_term)`. `get_scenes` every 500ms while catching up.

### 11.5 Catch-up

`have` is committed materialized head only: `{ room, n, hash, rpc_term }` with `rpc_term = commit_proof.rpc_term`. Every `heartbeat` **is** a `have`. Interval 500ms.

**Follower trigger (E1).** On `heartbeat`/`have`/`commit` with `n >` local committed head: the follower MUST `get_scenes(from=local_head+1, to=have.n)` and `apply(commit)` (then `advance_term`). Retransmit every 500ms until caught up. Heartbeats do not exempt a lagging follower from catching up; they only reset the election timer **after** the follower has started get_scenes (timer still resets so a lagging follower does not campaign while a live leader exists; progress is get_scenes, not election).

`get_scenes` returns `{scene, commit_proof}` for committed n. Receiver `apply(commit)` or `staged` if blobs missing, then `advance_term`.

If catch-up n equals pending.n: same hash → commit and clear pending; different hash → committed scene wins, unlink pending (including when the incoming file is `staged` pending blobs).

A higher `have.n` without bytes does not sicken and does not move head; it **does** start get_scenes.

## 12. Floor

The stick is in the ledger. You do not have the floor until a grant is committed.

### 12.1 Take states (holder node, per live grant)

| State | speak | yield |
|---|---|---|
| OPEN | append to buffer, assign `rev+1`, idempotent on `request_id` | freeze buffer at current rev, go CLOSING, ask leader to propose speech |
| CLOSING | reject `no_grant` (take is closing) | idempotent, same frozen rev |
| CLOSED | reject | reject |

`speak` request: `{room, text, request_id}`. Same `request_id` returns the same `{ok, grant_hash, rev}` without appending twice.

`speak` does **not** create a scene and does not return `n`.

**Freeze protocol (yield, timeout, yank).** The holder node is the source of frozen text. The leader must not empty-close while the holder is still OPEN.

1. Trigger: holder `yield`; or grant age ≥ `timeout_secs` (clock: leader's Unix time minus grant `ts`); or moderator `yank`.
2. Leader sends `freeze { room, grant_hash }` to `to.node`.
3. Holder, on `freeze` or local yield: if OPEN, fsync take state CLOSING with frozen `{text, rev, blobs[]}`, reject further `speak`, persist `close_take.json`. Reply `close_take { grant_hash, text, rev, blobs }` to the **current** `leader_id`.
4. Leader proposes `speech` with that text/blobs (fetch blobs before `precert` on followers).
5. Empty close is allowed only when `freeze` is **undelivered** (connection error / no freeze-ack) after `FREEZE_WAIT` (5s). If freeze was ACKed, the holder is CLOSING: wait for `close_take` until the holder disconnects, then empty-close. Never empty-close a still-OPEN holder that we can still talk to.
6. Holder that later sees the grant committed closed discards buffers (text may be lost if we empty-closed a dead holder). Logged.
7. Leader change: CLOSING holder retransmits `close_take` to the new leader. New leader uses that text if the grant is still live.

`close_take` and CLOSING state survive process restart (disk).

**request_id.** Scope `(room, grant_hash, agent, node, request_id)`. Retain until that grant is CLOSED. Hex, ≥16 random bytes. Same id: same `{ok, grant_hash, rev}`. After CLOSED, a reused id is a new speak on a later grant only if the grant_hash in scope differs.

### 12.2 wait-for-floor

Client blocking call. Returns the committed grant scene when:

1. Local node is `valid`
2. Live grant `to` equals this `(agent, node)`
3. Take is OPEN (not CLOSING)

`--timeout` on the client: `timeout` error. The floor itself still times out on `timeout_secs` from grant commit.

### 12.3 Intents

To be granted the stick you need a live intent. `wait-for-floor` sends `kind=wait` if this `(agent, node)` has **no unconsumed intent**, then blocks. It does not create a second intent for the same mouth. `raise-hand` sends `kind=raise` (supersedes that mouth's previous unconsumed intent).

```
{
  "v": 1,
  "id": "<32-byte random, hex>",
  "room": "<room id>",
  "kind": "raise" | "wait",
  "agent": "codex",
  "node": "<node id>",
  "ts": 1766700000,
  "exp": 1766701800,
  "sig": "<ed25519 over raw SHA-256 digest of JCS without sig>"
}
```

`exp` default `ts + 86400`. Floor `timeout_secs` is take duration, not queue TTL. `wait-for-floor` while already queued refreshes `exp` on that mouth's current intent (new signed copy, same `id` and `ts`).

Duplicate `id` is the same intent. **One unconsumed intent per `(agent, node)`.** A new intent for that mouth **supersedes** the old id (old id is cancelled, not grantable). Cancelled ids are not consumed-by-grant; they simply drop out of the queue.

Queue order is **deterministic and global**: sort unconsumed, uncancelled, unexpired intents by `(ts, id)`. Leader and followers compute the same order from the intent set they have. Leader gossips intents; a follower that lacks the intent bytes refuses `precert` on the grant. After commit, catch-up does not need the intent.

Expiry at certify time uses **`scene.ts < intent.exp`** (the grant's hashed `ts`), not the certifier's wall clock.

Grant body `intent_id` is the mouth's current unconsumed intent. `to` must match that intent.

`precert` of a grant additionally requires: among the intents **this follower has**, that are unconsumed, uncancelled, unexpired, `intent_id` is the minimum `(ts, id)`. If the follower is missing an earlier intent, it refuses; the leader retries after gossip. The queue is a function of the intent set plus consumed ids on chain, not of the chain alone. An honest leader proposes the min of intents it has; lagging followers delay the cert rather than fork the order.

Consumed on grant commit. Remaining intents stay queued.

There is no `reason=vacant` targeting an unpublished waiter. No waiter, no grant.

### 12.4 Who proposes floor scenes

Always the **consensus leader**, not every staker.

- Stick mode: leader proposes `grant` for the first queued unconsumed intent.
- Moderator mode: leader proposes `grant` only when it has received `grant_req` from the moderator mouth. Yank: `yank_req` from moderator, then freeze protocol. Non-moderator → `not_moderator`.
- Speech: freeze protocol (§12.1). `close_take` includes `blobs` already uploaded via `put_blob`.
- Membership and view-change: leader proposes when vacant, or as the holder's take (`closes_grant` set). Membership requests are accepted only from agents attached to roster nodes. Never "because we cannot reach majority."

**Client → leader.** `grant`, `yank`, `membership`, `breakout`, `yield`/`close_take` may be issued on a follower. That node sends the matching swarm `*_req` / `close_take` / `breakout_req` to `leader_id`. If `leader_id` is unknown: reply `unavailable`. The leader proposes; it does not require the client to be local.

Holders must be on roster nodes. An observer can watch, not speak.

### 12.5 Floor transition table

Let L = live grant present.

| Current | Scene | Next |
|---|---|---|
| empty | genesis | vacant, roster=[creator] |
| vacant | grant | L set |
| L | speech, breakout, membership with closes_grant, view-change with closes_grant | vacant (plus body effects) |
| vacant | membership without closes_grant | vacant, policy/mode updated |
| vacant | view-change without closes_grant | vacant, roster updated |
| L | grant | reject |
| vacant | speech / breakout | reject |
| any | scene that fails apply() | reject |

### 12.6 Live draft

Holder may gossip `draft {grant_hash, text, rev}`. UI may show it labeled live / unverified. `history` never includes drafts. On failed term or yank without buffer, drafts disappear.

### 12.7 Human

Agent id `human:<name>`, default `human:operator`. Same verbs.

## 13. Stake and roster

Joining the swarm is not jury duty.

### 13.1 Eligibility predicate

`role` is `stake` or `observe`, declared at join, carried in `hello` as a signed `decl {room, role, agents: [ids], ts, sig}`.

```
eligible(node) =
    role != observe
    AND (allowlist is empty OR node in allowlist)
    AND (
         (explicit AND role == stake)
         OR (agents AND len(declared agents) > 0)
        )
```

Default policy: `agents=true`, `explicit=true`, `allowlist=[]`. So a `--stake` join is eligible even with no agent yet, and a node that attached an agent is eligible even if it joined without thinking about role, unless it joined `--observe`.

`--observe` is never eligible. Nano always `--observe`.

**`conch create` cannot pass `--observe`.** Creator must certify genesis. Creator starts on the roster as stake.

**`conch join` default is `--stake`.** Auto-join on breakout is `--stake`.

Vote per node, floor per `(agent, node)`.

### 13.2 Roster mutation

Committed roster starts `[creator]`. Changes only via committed view-change.

**Signer-verifiable:** next_roster arithmetic, non-empty, add/remove disjoint, add ids well-formed, allowlist: an add that violates allowlist is illegal, an observe decl cannot be added.

**Proposer-only (CFT trust the leader):** liveness. Leader may `remove` a roster node after `REMOVE_AFTER` (300s) with no `hello`/`decl`, or immediately on signed `leave`. Followers do not re-check the 300s clock.

Add: leader proposes when `eligible` and node is `valid` (caught up) and connected. Must have a current `decl`.

Illegal: next_roster empty.

**Partition 4 nodes 2-2.** Majority 3. No new leader (need 3 votes). A pre-split leader on a two-node side may still `append`; those entries do not commit. No view-change commit.

**One dead of four.** Three can elect a leader and wrap (dead vote unused). Leader may later view-change remove the dead (still 3 certs).

**Two stakers, one dies.** Majority 2. No wrap, no view-change. Halt until return or human new genesis (new room id). v1 does not promote a singleton.

## 14. Breakout and bootstrap

**Cold start.** `conch create --name NAME` → genesis, ticket file, magnet with `g=`. Each machine `conch join <ticket>`.

**Lobby breakout.** Holder `conch breakout --name NAME [--members node,node]`. Leader proposes `breakout` body: new room key on the holder node, child genesis signed, ticket embedded, `parent` set, `auto_join` default = current roster, or the listed subset (must be subset of roster). Observers not listed.

On commit, each listed node joins the child locally (`role=stake`). Child genesis must `apply()` before child `wait-for-floor`.

**Fallback.** `conch join` a magnet from chat. Always legal.

## 15. Disk

```
~/.conch/
  node.key
  node.pub
  listen.json
  rooms/
    <room-id>/
      ticket.conch         # token stripped
      room.key             # creator / breakout holder only
      consensus.json       # term, voted_for
      pending.json         # uncommitted accept + own cert
      head                 # cache; scenes/ is source of truth
      current-room         # optional default id for CLI
      scenes/<n>-<hash>.json
      blobs/<sha256>
      intents/<id>.json
```

Restart: replay scenes through `apply()`, load consensus.json and pending.json. Side effects keyed by scene hash, skipped if already done.

## 16. Messages

Framed JSON, `typ` field. Blobs: `blob_meta` then raw length-prefixed bytes. Max 64 MiB.

**Swarm**

| typ | Fields |
|---|---|
| `hello` | node, pub, addrs, decl[] |
| `auth` | room, token |
| `pex` | peers[{node, addrs}] |
| `have` | room, n, hash, rpc_term (commit_proof.rpc_term) |
| `get_scenes` | room, from_n, to_n |
| `scene` | `{scene, commit_proof}` |
| `request_vote` | room, rpc_term, candidate, last_n, last_hash, last_rpc, sig |
| `vote` | room, rpc_term, voter, candidate, last_n, last_hash, last_rpc, grant, sig |
| `append` | room, rpc_term, leader, prev_n, prev_hash, scene |
| `cert` | room, n, hash, rpc_term, leader, node, sig |
| `commit` | room, n, hash, rpc_term, leader, certs[], scene |
| `heartbeat` | room, rpc_term, leader, n, hash, have_rpc  (committed have) |
| `nack` | room, have_n, have_hash, have_rpc |
| `intent` | intent object |
| `freeze` | room, grant_hash |
| `close_take` | room, grant_hash, text, rev, blobs |
| `grant_req` | room, to, from `{agent,node}` |
| `yank_req` | room, from `{agent,node}` |
| `breakout_req` | room, name, members?, from |
| `membership_req` | room, stake?, floor?, from |
| `draft` | room, grant_hash, text, rev |
| `leave` | room, node, sig |
| `get_blob` | sha256 |
| `blob_meta` | sha256, bytes |

**Client**

| typ | Fields |
|---|---|
| `attach` | agent |
| `create` | name, stake, floor (`moderator` required if mode moderator) |
| `join` | ticket or magnet, role (default stake) |
| `history` | room, from_n, follow |
| `wait_for_floor` | room |
| `speak` | room, text, request_id |
| `yield` | room |
| `raise_hand` | room |
| `grant` | room, to `{agent,node}` |
| `yank` | room |
| `breakout` | room, name, members? |
| `membership` | room, stake?, floor? |
| `put_blob` | name, bytes meta + raw |
| `leave` | room, vacate: bool |
| `status` | room? |

Replies: `{ok, data}` or `{ok:false, error:{code, message}}`.

Codes: `no_grant`, `sick`, `unknown_room`, `bad_ticket`, `unauthorized`, `timeout`, `invalid`, `not_moderator`, `unavailable`.

`history` for agents: committed scenes only. While catching up, `data.syncing=true` and a verified prefix. That is not a violation of invariant 4: prefix is verified, drafts are absent, `complete` is false. `wait_for_floor` requires `valid` and a live OPEN grant; it does not require majority connectivity (if there is no leader, it simply does not return).

## 17. Transports

Core does not branch on scheme. v1 adapters:

| Scheme | Role |
|---|---|
| `ws://` `wss://` | UI, cafe laptop, EC2 → nano. `/swarm`, `/client` |
| `tcp://` `tcps://` | LAN mesh, CLI default. `u32be length \| json` |
| `http://` `https://` | `GET /ticket/:id` (auth if token), `GET /history/:id?from=0` (auth), `GET /ui/` |

Plain schemes on loopback/LAN. Public FQDN: TLS. `--tls-cert` `--tls-key`. Clients: platform CA, or `--tls-ca`. `tcps` = TCP frames inside TLS.

Dial trackers, then peers, then PEX. One successful hello+genesis starts catch-up. Mesh among reachable pairs.

Kill nano: LAN tcp PEX keeps wraps. Cafe laptop with only the nano waits. Path loss, not ledger loss.

PEX, history, blob, get_scenes require a successful `auth` for that room when `token_sha256` is set.

## 18. CLI and MCP

```
conchd [--data-dir DIR] [--http ADDR] [--tcp ADDR] [--localhost] [--tls-cert F] [--tls-key F] [--tls-ca F]

conch [--node tcp://127.0.0.1:7421] [--agent NAME] [--room ID] <cmd>
```

`CONCH_NODE`, `CONCH_AGENT`, `CONCH_ROOM`. Default node `tcp://127.0.0.1:7421`. Default agent `local`. `--room` required if more than one room is joined and `CONCH_ROOM` / `current-room` is unset.

```
conch create --name NAME [--mode stick|moderator] [--moderator agent --moderator-node id]
conch join <ticket> [--stake|--observe]          # default --stake
conch history [--follow] [--from N]
conch wait-for-floor [--timeout secs]
conch speak [--file -] [--request-id ID]
conch yield
conch raise-hand
conch grant --agent NAME --node ID               # moderator
conch yank                                       # moderator
conch config [--mode stick|moderator] [--moderator agent --moderator-node id] [--stake-json JSON]
conch breakout --name NAME [--members id,id]
conch blob put FILE
conch leave [--vacate]
conch status
conch mcp
```

`create` writes `./<slug>.conch` (includes token if any) and prints `{ticket_path, magnet, id}`. Magnet includes `g=` and uses scheme `conch:1:`.

`speak` prints `{ok, grant_hash, rev}`. `yield` prints `{ok, grant_hash, rev}`.

MCP tools: same verbs with underscores, including `grant`, `yank`, `config`, `blob_put`.

Skill: you have the `conch` CLI. join, history, raise-hand, wait-for-floor, speak, yield. Do not invent turn-taking. `no_grant` → raise or wait. Moderator-only verbs are for the human or the named moderator mouth.

## 19. Auth and threat model

Crash-fault operator machines plus a capability.

- Node key signs votes, certs, intents, decls, leave (always raw 32-byte SHA-256 digest).
- Room key signs genesis only.
- Ticket token: SHA-256 of raw 32 bytes compared to genesis `token_sha256`.
- TLS when TLS schemes are used.
- Agent string is bound only to "this node attached it."

Invariant 1 does **not** hold if a roster node equivocates (signs two logs in the same term, or two votes). Honest `conchd` will not. A forked binary can produce two majorities with a 3-node roster `{A,B,M}` by lying to A and B. v1 documents that and does not claim BFT. Later wrap-engine swap may.

## 20. Failure

| Event | Behavior |
|---|---|
| Majority unreachable | No leader. No wrap. Valid prefix still readable. |
| Drop mid-cert | Unused vote. pending.json plus same-term different-hash refusal prevent a second hash in that `rpc_term`. |
| Leader crash after majority cert, before gossip | Voters' tails include pending H. New leader is one of those voters and re-appends exact H. No H′. |
| Catch-up of a high proof rpc_term | `advance_term`; cannot campaign below that floor. |
| Process crash after cert, before commit | pending.json is the tail (same hash). Startup scan recovers if the scene file already exists. |
| Nano killed | PEX seeders continue. |
| Torn/unreadable scene file on scan | Treated as absent. `sick` for signing only if the hole is in `0..=head` after scan (cannot verify a committed prefix). Re-leech. No merge. |
| Holder dies OPEN | timeout_secs, leader empty-closes. |
| Two stakers, one dies | Halt. |
| Blob missing at cert time | Do not certify. |
| CLI cannot reach conchd | `unavailable`. |

## 21. Worked examples

**A. Three minis, two agents, stick.** Roster `[m1,m2,m3]`. Codex on m2 raises (I1). Claude `wait-for-floor` creates I2 (Claude has no intent yet). Leader grants `(codex, m2)` because I1 sorts first. Codex speak/yield. I1 consumed. I2 still queued, so Claude is next. Codex must raise or wait again and lines up **behind** Claude.

**B. Kill nano.** Four staker minis PEX'd LAN tcp. Nano observer dies. Election and wraps still have majority 3 of 4. Cafe laptop using only the nano waits.

**C. Late MacBook.** Joins via tracker or peer hint. apply() each leeched scene. Until valid, no certs, wait-for-floor blocks. Leader may view-change add if eligible.

**D. Breakout.** Human on a roster node holds grant. `breakout --name fluid`. Commit. Four roster minis auto-join child as stake. Nano not on auto_join. Human opens child.

**E. Poison.** Mini-3 flips a byte in scene 4. apply() fails. Mini-3 does not certify. Agent history on mini-3 errors `sick` or returns last valid prefix with syncing. Re-leech scene 4. Recovers.

**F. Partition 2-2.** Roster of 4. Neither side collects 3 votes for a new term. A pre-partition leader may append on its two-node side; that stays accepted, not committed (only 2 current-term certs). After heal, that leader resumes if `rpc_term` is still highest; otherwise a new election, winner's tail includes any majority-accepted H, re-appends H, collects new current-term certs, commits H. No H′.

**G. Restart mid-accept.** Node accepted H@n at `accepted_rpc_term=4`, crashed. If H was not committed, tail is pending H; a candidate at n-1 loses; a new leader that also has H re-appends H and recerts. If the scene file exists on disk, startup scan recovers committed head and unlinks pending (`pending.n <= head.n`).

**H. Majority accept, leader dies before commit push.** Roster of 5. A, rpc_term 2, appends H@n. B,C,D fsync pending H and cert (rpc_term 2, leader A). A fsyncs commit_proof locally, dies before `commit` gossip. E campaigns at rpc_term 3; B/C/D process that `request_vote`, set `current_term=3`, refuse E (tail n-1). B then campaigns at **rpc_term 4**, tail H, wins. B probes `have`: C/D still at n-1, so B re-appends **exact hashed H**, collects **new** certs bound to (rpc_term 4, leader B) — not A's term-2 certs. Majority → commit_proof at term 4, `commit` push (includes `room` + `scene`). A returns with H already committed (term-2 proof). Same hash. No n+1. No H′.

**H2. A pushed commit to C,D then died (D2).** C/D `have` n=H. B pending H, committed n-1, wins. Probe sees C/D at n same hash → fetch proof, `apply(commit)`, `advance_term`, do not append with `prev_n=n-1`. No deadlock.

**I. Term floor (v1.4 fork).** P@n-1 committed with `commit_proof.rpc_term=100`. A,B,C,D install that proof and `advance_term(100)`. They cannot campaign at 2; first legal campaign is ≥ 101. If S is the term-101 leader, S needed a majority of the same roster at height n-1, so at least one of A's voters already has `current_term>=101` and a durable `voted_for` (cleared only by a **strictly higher** floor). A cannot also win 101. A retries at ≥ 102. Whoever wins ≥ 102 either carries an accepted H if one exists, or mints H with no pending. S's stale tail `(100, n-1, P)` or `(101, n, H′)` loses on `last_rpc` to every intersecting H holder. The equal-rpc-equal-n hash-refuse branch is not the one that fires. One committed hash at n.

## 22. Testing

- 3-node commit; 2-2 of 4 commits nothing; heal then one head.
- Example H: E's campaign is term 3 and is refused; B wins term 4; new certs; commit H in place.
- Example H2: C/D already committed; B installs their proof, does not append prev=n-1.
- Example I: after proof 100, A cannot win ballot 101 against S's voters; later ballots prefer H holders on `last_rpc`. Not the equal-n hash-refuse branch.
- Win abort: `advance_term` during probe demotes the winner; no append at the new term without a new election.
- E1: lagging C on heartbeat n>head get_scenes; append nack carries have.
- E2: leader removed by view-change pushes commit to next_roster majority, then steps down.
- E3: startup unlinks pending.n ≤ head.n.
- Same rpc_term, different hash: refuse (protocol violation).
- Removed node request_vote ignored.
- Carry-forward grant: skip intent min-queue precert; still committable if queue order drifted.
- Inherited H is a grant: next scene after **commit** of H may be speech (floor live). No barrier scene.
- Inherited H is a view-change: apply(commit) updates roster; next scene uses new roster. `|add|+|remove|==1`.
- Zombie old leader ignores new certs (wrong `leader`/`rpc_term` on cert).
- Catch-up with only commit_proof (no live intents) succeeds. Pending same n cleared on commit.
- Crash after cert: tail is H; restart resends signed cert.
- Catch-up rejects self-appointed roster `[attacker]` with one cert.
- apply() `commit` vs `precert` vs `staged`: history/have ignore staged.
- apply() on disk replay equals live state.
- Two waits: only queue head is granted. `speak` without OPEN → `no_grant`. Closing then extra speak rejected. speak request_id idempotent.
- Grant without intent bytes: followers refuse cert.
- Blob missing: no cert. After blob, cert. Kill blob origin after commit: another node still valid.
- Observer never in certs, never voted_for, never grant.to.node.
- `create --observe` rejected. `join` default stake.
- Moderator: non-moderator grant → `not_moderator`. yank closes take.
- CLI `--room` required with two rooms. Magnet without `g=` rejected.
- GET /ticket without bearer, tokened room → 401.
- Kill tracker after PEX; wraps continue.
- Breakout auto-join shares child genesis. Manual magnet still works.
- CLI and MCP: create, join, history, wait, speak, yield, grant, yank.
- UI: wrapped transcript, live draft labeled, compose only when OPEN.
- TCP and WS commit the same hashes. HTTP history matches CLI.

## 23. Build order

1. Scene encoding, apply() on disk, genesis, poison/valid (single node).
2. TCP, tail-aware votes, current-term certs, commit push, Example H, 2-2 freeze. No noop.
3. Intents, grant/speech, wait-for-floor, speak/yield states, CLI localhost.
4. Stake predicate, observer, view-change, kill-tracker.
5. WS + HTTP, ticket/magnet parser, create/join, token auth.
6. Web UI.
7. MCP + skill. Breakout auto-join. Moderator verbs. Blob put.

## 24. Decisions (do not reopen in the first plan)

- Torrent-shaped room. Not libtorrent, not Iroh, not Ethereum.
- Wrap engine is Paxos-style majority of current-term certs among the roster. Still not a public L1. Still no master machine.
- CLI and MCP both first class.
- Three transports: WS, TCP, HTTP (plain and TLS schemes).
- All three stake paths, with the boolean predicate in §13.1.
- Breakout is a wrapped scene; pasted magnet is fallback.
- Full replica on every node. History is the chain.
- Grant is committed before `wait-for-floor` returns.
- Nano is observer, tracker, full replica, never voter, never leader.
- Roster changes only via committed view-change.
- Blockchain means hash chain plus quorum certificates.
- Product name is **Conch**. Domain noun remains **room** (ticket, ledger, `--room`). Binaries `conchd` / `conch`. Magnet `conch:1:`. Ticket file `*.conch`. Data dir `~/.conch`. JSON field `room` and genesis cert id `room` are protocol, not the product name.
)

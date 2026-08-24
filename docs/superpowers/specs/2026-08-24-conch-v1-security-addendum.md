# Conch v1 security addendum — review draft

**Status:** normative for the production-v1 implementation after Claude and Grok review. It extends spec v1.6 §§16–19. It does not change §11 consensus or §24 product decisions.

This addendum explicitly overrides §17's default bind: the default is now literal loopback, and a non-loopback listener requires `--mode lan` or `--mode public`. It also overrides §8's creation default: `conch create` creates a private room unless `--open` is supplied. Documentation and tests that assumed `0.0.0.0` or a tokenless room by default are superseded.

## 1. Release boundary

Listener modes are mutually exclusive:

- `--mode local` is the default (`--localhost` is an alias). Both listeners are forced to literal loopback and use plain TCP/HTTP/WS.
- `--mode lan` permits explicit non-loopback plain listeners for a trusted network/overlay. It is never advertised as safe for an untrusted network.
- `--mode public` requires `--tls-cert` and `--tls-key`. The existing `--tcp` address becomes TCPS; the existing `--http` address becomes HTTPS/WSS. Native TLS termination is mandatory; reverse-proxy TLS termination is not a supported v1 mode.

Startup fails closed for incompatible flags, unreadable material, a certificate/key mismatch, or a non-loopback bind in local mode.
- `tcps://`, `wss://`, and `https://` use TLS 1.3 and never downgrade to plaintext. Clients validate hostname and a platform CA by default, or the CA supplied by `--tls-ca`. `--tls-ca` is outbound server/CLI trust configuration, not client-certificate authentication.
- Advertising a secure scheme without the corresponding secure listener is a configuration error.

Outbound policy is mode-bound for every endpoint source, including ticket peers/trackers, PEX, redirects, and explicit CLI targets. Public mode accepts and dials only `tcps://`, `wss://`, and `https://`. Local mode permits plaintext only to literal loopback. LAN mode permits plaintext only after the operator explicitly selected LAN mode. No mode falls back across these classes, and a capability is never sent to an endpoint disallowed by the active mode.

A request carrying a room capability or session credential does not follow a cross-origin redirect and never forwards `Authorization` or `Cookie` to a redirect target. It may follow at most five redirects whose canonical origin exactly equals the original origin and whose endpoint remains allowed by the active mode. Requests without credentials still obey the mode matrix and five-hop limit.

## 2. Connection-bound node authentication

The reusable declaration signature is not proof that its node owns the current connection. No room, election, replication, floor, blob, PEX, or declaration message is processed until this handshake completes.

1. Initiator sends `hello_i {label:"conch-swarm-v1",kind:"hello_i",v:1,node,pub,nonce_i,sig}`. The signature is Ed25519 over the raw 32-byte `SHA-256(JCS(payload without sig))`.
2. Responder verifies `node==pub` and the signature, then sends `hello_r {label:"conch-swarm-v1",kind:"hello_r",v:1,node,pub,peer,nonce_i,nonce_r,hello_i_hash,sig}`. `peer` is the initiator. The signature uses the same digest rule.
3. Initiator verifies `sig` over the raw 32-byte `SHA-256(JCS(payload without sig))` against `hello_r.node`/`pub` before any other field is trusted, then verifies `node==pub`, `peer`, `nonce_i`, and `hello_i_hash`, then sends `hello_ack {label:"conch-swarm-v1",kind:"hello_ack",v:1,node,peer,nonce_i,nonce_r,hello_i_hash,hello_r_hash,sig}`.
4. Responder verifies `hello_ack.sig` over the raw 32-byte `SHA-256(JCS(payload without sig))` against `hello_ack.node` before any other field is trusted, then verifies that `node`, `peer`, and both nonces match this connection. Only then is the peer identity authenticated.

Both roles reject a handshake as soon as the claimed remote node equals the local node id. The responder checks `hello_i.node` before sending `hello_r`; the initiator checks `hello_r.node` before sending `hello_ack`. Rejection sends no room authorization or capability and closes the connection.

`hello_i_hash` and `hello_r_hash` are their signed-object digests. Nonces come from the operating-system CSPRNG, are generated per connection, and are never persisted or reused. A prior transcript is invalid on a new connection. Endpoint advertisements are not part of the identity handshake.

A receiver rejects any handshake frame whose `label` is not exactly `conch-swarm-v1` or whose `v` is not exactly `1`. It accepts only the `kind` expected in the current state: `hello_i`, then `hello_r`, then `hello_ack`. An unexpected or repeated step, an unknown version, or another handshake frame after completion closes the connection. V1 does not negotiate down to another protocol version.

If an initiator receives `hello_i` instead of `hello_r`, the peer with the lexicographically smaller node id becomes responder for that TCP connection: it processes the inbound `hello_i` and sends `hello_r`. The peer with the larger node id stays initiator on that socket and waits for `hello_r`. Do not run two handshakes to the same peer id on two sockets; keep one.

The first pre-auth frame limit is 64 KiB and the complete handshake must finish within 5 seconds.

## 3. Room authorization and disclosure

`hello` is identity-only. It contains no room declarations or agent ids.

Room authorization is per connection and per room. One successful exchange authorizes both directions. Only the node-handshake initiator sends `auth {room, token?, declaration}`. The responder verifies `auth.room == auth.declaration.room`, that the declaration signer is the authenticated initiator, and the capability against its committed genesis. A token is mandatory when genesis has `token_sha256`; it is omitted, never `null`, for an open room. The responder marks the room authorized immediately before sending exactly one `authed {room, declaration}` whose declaration names that room and is signed by the authenticated responder. The initiator verifies all three bindings and marks the room authorized only afterward. A duplicate identical `auth` is idempotent; a conflicting duplicate or any room message before authorization closes the connection. Reconnect starts unauthorized.

Before `authed`, neither side sends or accepts room-scoped PEX, `have`, scenes, blobs, intents, floor messages, election messages, or roster admission.

`pex` includes `room`; peers and endpoints are accepted only for an authorized room. Per room/connection: at most 256 peer entries, at most 8 endpoints per peer, at most 2 KiB per endpoint, and only schemes enabled by this build. Invalid or pre-auth PEX is rejected and not persisted.

When dialing an endpoint stored for node `N`, the authenticated responder must be `N`; otherwise the connection is closed and no endpoint/message is accepted.

Every sender-bearing message is bound to the authenticated node:

| Message | Field that MUST equal the session peer |
|---|---|
| `request_vote` | `candidate` |
| `vote` | `voter` |
| `append`, `heartbeat` | `leader` |
| `cert` | `node` |
| `leave` | `node` |
| unsigned floor control / forwarded request | `node` or `from.node` |
| `auth` declaration | declaration signer |

An intent is independently signed: its signature is verified and its `intent.node` must match that signer. It may be relayed by any peer authorized for the room. Unsigned floor-control payloads—including `freeze`, `close_take`, `grant_req`, `yank_req`, `breakout_req`, `membership_req`, and a forwarded `leave`—must bind `node` or `from.node` to the session peer. Independently signed fields are still verified.

`scene` and `commit` may be relayed because their proofs authenticate their contents, but a record for room R is accepted only on a connection authorized for R. Authorization for one room never grants relay authority for another.

Roster admission may run only for the authenticated declaration and only on the current leader. Existing roster declarations take a lock-free fast path; a new admission remains serialized with other room mutations.

In `--mode public`, a tokenless (`--open`) room MUST NOT be authorized for any peer, including observers. `conchd` MUST refuse to advertise or replicate an open room in public mode (startup error if an open room is loaded, and `auth` rejected). HTTP ticket/history for those rooms MUST fail closed in public mode. `--open` is local/LAN only. An already-committed roster node of a private room may vote/certify after proving its node identity and presenting the room capability. Capability-free stake admission is limited to LAN mode.

## 4. Browser client sessions

WebSocket client authorization never derives from the backend TCP source address and never places a capability in a URI.

- `POST /session/:room` accepts a Bearer room capability, validates a canonical same-origin `Origin`, and returns a room-scoped opaque session cookie. It works over HTTPS and literal-loopback HTTP. An absent, `null`, malformed, or cross-origin `Origin` is rejected.
- No browser write session is minted for a tokenless open room in any mode. This removes source-address, Host-header, and reverse-proxy trust from browser authorization.
- The UI hides or disables compose, raise-hand, yield, moderator, membership, and breakout controls for a tokenless open room and labels that browser view read-only. Loopback CLI access remains available under §5.
- `/client` requires a valid session cookie and matching same-origin `Origin`; every request is restricted to that session's room. Unknown, expired, cross-room, and cross-origin sessions are rejected.
- The UI keeps a capability in memory only long enough to exchange it for the cookie. It does not put it in a query string, browser URL, `localStorage`, or logs.
- Public HTTP ticket/history reads follow genesis capability policy. Private responses use `Cache-Control: no-store`. The served ticket never contains the raw token.

Session identifiers are 32 random CSPRNG bytes. The server stores only their SHA-256 hashes, compares hashes in constant time, binds each to `{room, canonical_origin}`, expires them after 15 minutes absolute, caps the table at 4096 live sessions, drops expired entries before LRU eviction, and invalidates them on restart. Accept exactly one RFC 6454 tuple origin. Reject multiple/comma-list values, opaque/`null` origins, userinfo, path, query, fragment, and non-ASCII host input. Lowercase scheme and the ASCII/IDNA host, preserve a terminal DNS dot as a distinct host, and insert the effective port. Compare the `(scheme,host,port)` tuple exactly with the validated request-target authority; never use prefix/suffix/substring matching and ignore `Forwarded` and `X-Forwarded-*`. `localhost`, `127.0.0.1`, and `[::1]` remain distinct, and a session is valid only for the origin that minted it. `DELETE /session/:room` revokes the presented session. TLS uses cookie `__Host-conch_session` with `Path=/; HttpOnly; SameSite=Strict; Secure` and no `Domain`; loopback HTTP uses `conch_session` with the same attributes except `Secure`. Session ids and cookies are redacted like room capabilities.

## 5. Local CLI clients

Plain TCP `attach` is accepted only on a loopback listener from a loopback peer. Public and trusted-LAN TCP listeners are swarm-only. CLI and MCP operate through a daemon local to the user in v1. A separate room-scoped remote client adapter is out of v1 scope; source IP never grants browser or CLI authority.

## 6. Secret handling and filesystem modes

- On Unix, the data directory and room directories are `0700`; node keys, room keys, local join state, takes/drafts, pending state, consensus state, tickets containing capabilities, and other private room files are `0600`.
- `conch create` does not print a raw token or token-bearing magnet by default. Secret output requires `--show-secret`; noninteractive secret input supports `--token-file FILE` and `--token-file -`.
- New rooms are private by default with a generated capability of exactly 32 bytes from the operating-system CSPRNG, rendered as 64 lowercase hexadecimal characters. It is never derived from the room/name, node key, time, counters, or process PRNG state. `--open` explicitly creates a tokenless room. CLI and MCP results omit secret material unless their request explicitly sets `show_secret=true`. The normal capability-bearing handoff artifact is the `./<slug>.conch` file written by `conch create`; it is mode `0600` and contains the token. Stored `rooms/<id>/ticket.conch` files and every served `GET /ticket/:id` response are token-stripped. `create` prints a token or token-bearing magnet only with `--show-secret`.
- Tokens and authorization headers are redacted from errors and logs.
- The browser never persists the room token. Authenticated HTTP responses use `no-store`.
- A breakout capability embedded in the parent ledger is explicitly shared with every authorized parent-history reader; `auto_join` is convenience, not an access-control list.
- V1 room capabilities do not rotate. A suspected leak requires creating a new room and moving participants; browser sessions can still be revoked or expire normally.

On startup, Unix implementations tighten existing data/room/staged-breakout directories and private files to these modes before serving. A path that cannot be made private is a startup error. TLS private keys must already be `0600` or stricter and are never rewritten.

## 7. Resource bounds

- Defaults are: 1024 concurrent connections globally, 64 per source IP, 5-second TLS/node-handshake timeout, 30-second read-idle/write timeout, 64 KiB pre-auth frame maximum, and the existing 64 MiB post-auth frame maximum. Limits may be lowered by flags but never raised above these hard maxima in v1. Excess work is rejected before allocating a full frame.
- No pre-auth allocation exceeds the 64 KiB pre-auth frame limit.
- Every per-connection inbound or outbound application queue is bounded to 64 frames and 64 MiB of encoded payload in aggregate, whichever is reached first. Exceeding either closes the slow connection; unbounded channels are forbidden.
- PEX caps are cumulative unique values after canonical merge: at most 256 nodes per authorized room/connection and in persisted room state, and at most 8 endpoints per node. At most 8 PEX-triggered dials per room and 64 globally are outstanding. Rejected, duplicate, and over-limit endpoints create no task and are not persisted.
- Sync is deduplicated per room, bounded by timeout, and one stalled peer cannot create one task per heartbeat.
- Node-handshake, room-auth, and browser-session authentication failures share a per-source token bucket of 20 burst and 10/minute refill; excess attempts are rejected without credential logging. Session-id and capability comparisons are constant-time.

## 8. Release-gating tests

1. Replayed hello and complete old handshake cannot advance term, obtain a cert, or change `consensus.json`.
2. Tampering with handshake version, kind, node, peer, either nonce, or transcript hash fails connection authentication; tampering with a room declaration fails room authorization.
3. A legitimate handshake permits election, append, commit, catch-up, and floor traffic.
4. Pre-auth peers receive no room declaration, agent id, PEX, `have`, scene, or blob and cannot trigger admission.
5. Pre-auth/oversized/malformed PEX is rejected without persistence or outbound dialing.
6. Remote no-token WebSocket mutation, evil-origin loopback use, and reverse-proxy loopback bypass are rejected.
7. Browser capability auth succeeds without the token appearing in the request target or persistent browser storage.
8. Ticket/history bearer matrix and `Cache-Control: no-store` pass.
9. HTTPS/WSS/TCPS succeed with trusted roots/custom CA and matching hostname; untrusted/wrong-host/expired credentials and downgrade attempts fail.
10. Unix permission tests pass under umask `022`; default command output and captured logs contain no sentinel capability.
11. Slowloris, connection-limit, frame-limit, PEX-limit, and sync-deduplication tests show bounded tasks and memory.
12. Sender-field/session-node mismatches, expected-peer endpoint substitution, tokenless public stake admission, and a three-node replayed-hello forged-quorum trace are rejected without term movement or durable state change. This tests replay resistance under the §7 crash-fault/non-equivocating model; it does not claim Byzantine protection against a forked majority.
13. Session cookie name/path/flags, entropy, origin binding, expiry, restart invalidation, explicit revocation, and table cap pass for TLS and loopback HTTP.
14. Startup migrates existing `0755`/`0644` room state to private modes or fails closed.
15. A live two-connection reflected/self-directed handshake yields no `auth`, term movement, floor mutation, or durable state change.
16. Same-origin redirects work within the five-hop bound; cross-origin secure and secure-to-plaintext redirects carrying a sentinel capability fail without forwarding credentials.
17. Slow-reader queues and repeated valid PEX batches remain within the cumulative queue, persistence, and dial bounds.
18. `--mode public` refuses to load, create, authorize, or HTTP-serve a tokenless (`--open`) room. Simultaneous `hello_i` on one socket completes with the lexicographically smaller node as responder.

# Conch local operator UI

Status: accepted implementation contract for the post-v1.0.1 operator-console update.
The consensus, floor, ticket, and product decisions in the v1.6 design remain unchanged.

## 1. Problem

The v1.0.1 web UI is a room-scoped transcript/composer. It does not expose the
multi-room state already persisted by `conchd`, has no participant view or room
creation path, has no deep links, forgets a selected room after transient client
failures, and forces the page toward the latest scene every 1.5 seconds.

The result is usable as a protocol smoke test but not as the local operator's
daily room console.

## 2. Security boundary

The UI has two explicit scopes.

1. **Local operator.** Available only when the daemon is in `local` transport
   mode and its HTTP listener is bound to a literal loopback address. An exact
   same-origin POST mints a short-lived opaque `HttpOnly; SameSite=Strict`
   operator cookie. The session authorizes catalog reads for rooms already
   stored by this daemon, room creation/join, and a private-room-bound client
   WebSocket. Tokenless rooms remain read-only in the browser, preserving the
   v1 security addendum. The session never returns stored room capabilities.
2. **Room session.** The existing ticket/capability exchange remains unchanged
   for LAN/public access. It authorizes exactly one room and cannot enumerate
   the daemon's catalog.

Operator endpoints do not exist on LAN/public HTTP listeners. Mutating operator
requests and the operator WebSocket require the exact canonical `Origin` that
minted the cookie. An unrelated local process is already inside the v1 local
CLI trust boundary; an unrelated web origin is not.

No room token appears in a URL, cookie, catalog response, `localStorage`, log,
or persisted UI preference. A token returned while creating a room exists only
long enough for the browser to download the new `.conch` file. All operator
responses use `Cache-Control: no-store` and same-origin resource policy.

## 3. Information architecture

Desktop uses three regions:

- **Rooms rail:** searchable local room catalog, create/join actions, room
  health/sync/head/role, and unread badges.
- **Conversation:** room header, floor state, committed transcript, live draft,
  and composer.
- **People rail:** participants grouped by node role, attached/historical agent
  mouths, and leader/floor/moderator badges.

With no selected room, the conversation region is a useful dashboard: create a
room, join from a ticket/magnet/URL, or select a local room. On narrow screens,
rooms and people become independently toggleable panels without hiding the
conversation controls from keyboard or screen-reader users.

## 4. Local room catalog

The catalog contains every room currently loaded from this daemon's data
directory. It never discovers or lists unjoined network rooms.

Each item contains:

```json
{
  "id": "64-hex",
  "name": "room name",
  "role": "stake|observe",
  "head_n": 12,
  "last_activity": 1787590000,
  "syncing": false,
  "valid": true,
  "roster_size": 3,
  "floor": { "state": "vacant|held", "agent": "codex", "node": "64-hex" }
}
```

Catalog order is descending `last_activity`, then room id. Search matches name
and room id. Selected room and search text are UI state; only the last selected
room id may be persisted, never authorization material.

## 5. Room detail and participants

The authoritative staker set is the committed roster. Observer presence is a
recent, verified room declaration known to this node. Agent is a mouth, not a
node role.

Each participant contains node id, `stake|observe`, known agents, last-seen
time, and booleans for local node, connected/recent, consensus leader,
floor-holder node, and moderator node. Historical grant recipients supplement
agent names when a live declaration is unavailable. Absence of an agent list is
rendered as “No attached agents observed,” not as a protocol role.

The UI always labels committed history separately from live/unverified drafts.

## 6. Deep links and navigation

Canonical local room URLs are `/rooms/<room-id>`. The route contains the room id
only. Direct load, refresh, back, and forward restore selection.

- Valid operator or room session: connect and render the room.
- Transient connection failure: retain route/selection and reconnect with
  bounded exponential backoff.
- Expired room-scoped session: retain the route and show a locked reauthorize
  state. Do not delete the selected room.
- Unknown/unavailable room: show an inline not-found state with a path back to
  the room library.

## 7. Transcript update and scroll algorithm

History refresh is incremental from `head_n + 1`; identical refreshes do not
rebuild the DOM.

Before appending, compute whether the transcript scroll container is within 48
pixels of its bottom. After appending:

- if it was near bottom, keep it at latest;
- if it was not, preserve the viewport and increment a floating
  **“N new messages”** pill;
- clicking the pill smoothly scrolls to latest and clears the count;
- opening a room initially scrolls to latest without moving the whole page.

No polling pass calls element-level `scrollIntoView`. User scroll position never
changes when no new committed scene arrived.

## 8. Operator API

All JSON responses are `no-store`.

```text
POST   /operator/session                 mint local operator cookie
DELETE /operator/session                 revoke it
GET    /operator/rooms                   catalog
POST   /operator/rooms                   create private room
POST   /operator/rooms/join              join ticket as stake/observe
GET    /operator/rooms/:id               room summary + participants
GET    /operator/rooms/:id/history?from=N committed verified page
GET    /operator/client/:id              room-bound operator WebSocket
GET    /rooms/:id                        serve UI deep link
```

Create accepts `{name, mode?, timeout_secs?}`. Private capability generation is
the same OS-CSPRNG 32-byte rule as CLI create. It returns the newly created
ticket once so the UI can download `<slug>.conch`; catalog/detail never returns
it. Join accepts `{ticket, role}` and runs the same ticket validation and catch-up
path as the CLI. A successful operator create or join selects that room as the
daemon's current room, matching the existing CLI create/join behavior.

## 9. Accessibility and interaction

- Room and participant rails have named navigation regions.
- Current room uses `aria-current="page"`; unread counts have accessible names.
- Dialogs trap focus, close on Escape, restore focus, and expose validation via
  a live region.
- The new-message pill is a button and its count is announced politely.
- All icon-only controls have names and at least 44x44 CSS-pixel targets.
- Motion honors `prefers-reduced-motion`.

## 10. Release gates

- Operator endpoints are unavailable outside local mode and reject wrong
  origins/cookies.
- Catalog/detail/history never serialize a token.
- Browser create returns one token-bearing ticket and the persisted ticket is
  stripped.
- Direct `/rooms/<id>` load and refresh retain the room.
- A failed/reconnecting WebSocket does not erase selection.
- Polling unchanged history preserves scroll exactly.
- New history while scrolled up shows the pill; clicking reaches latest.
- Stakers, observers, agents, leader, moderator, and holder render distinctly.
- Existing room-scoped browser, CLI, MCP, HTTP, and swarm security tests remain
  green.

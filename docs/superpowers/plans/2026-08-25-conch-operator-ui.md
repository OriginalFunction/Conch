# Conch operator UI implementation plan

Source: `docs/superpowers/specs/2026-08-25-conch-operator-ui.md`.

## 1. Local operator boundary

- Extend HTTP state with an explicit loopback/local operator capability.
- Add opaque operator sessions with bounded table, absolute expiry, constant-time
  validation, exact origin binding, and secure cookie attributes.
- Gate every `/operator/*` endpoint and WebSocket before room lookup or output.
- Add cross-origin, LAN/public, expiry, and non-disclosure tests.

## 2. Catalog and room detail

- Add daemon snapshots for local rooms, joins, replay/head/floor state, roster,
  last-seen nodes, verified declarations, and historically observed mouths.
- Record verified declarations after room authorization and refresh last-seen on
  authorized traffic without increasing the existing persistence cadence.
- Expose catalog, detail, and incremental history handlers.

## 3. Create and join

- Create private rooms with OS-CSPRNG capability and existing genesis path.
- Return a one-time ticket download response without storing its token in UI
  state after download.
- Route join through the existing ticket parser/validation/catch-up code.
- Test stake/observe role behavior and token stripping on disk/catalog.

## 4. Operator console frontend

- Replace the single-room sidebar with rooms/conversation/people regions.
- Add dashboard, create/join dialogs, search, room switching, responsive panels,
  and loading/empty/error/locked states.
- Implement `/rooms/<id>` history navigation and operator/room-session connection
  selection.
- Preserve the current floor-gated composer and live-draft labeling.

## 5. Incremental transcript

- Key rendered scenes by height/hash and request history from the next height.
- Implement near-bottom detection, unread accounting, floating latest button,
  and reconnect backoff.
- Add deterministic browser-independent JS tests for navigation and scroll
  decisions plus live Playwright coverage.

## 6. Verification and release

- Run formatting, workspace tests, Clippy, packaging, and integration validators.
- Exercise create/join/switch/refresh/scroll/participants in a real browser.
- Install the release build into the local launchd service and verify the
  existing collaboration room survives.
- Commit and push Bitbucket/GitHub, publish the next release, refresh Homebrew and
  website installer metadata, and verify public installation paths.

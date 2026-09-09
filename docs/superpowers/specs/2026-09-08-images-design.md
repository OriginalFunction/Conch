# Images in Conch — design

Date: 2026-09-08
Status: approved (Ray, 2026-09-08). Design page:
https://claude.ai/code/artifact/94cace53-dbef-4b31-ba75-68ac444f7bff

## Goal

Let a person on the operator console and an agent in a room share screenshots and
photos as part of a take, and let every participant see them.

Conch already replicates blobs: a speech scene carries
`blobs: [{name, sha256, bytes}]`, the bytes live under `blobs/<sha256>` in the
room's store, and no node certifies a scene until every referenced blob is on
disk with a matching length and digest. Agents attach with the MCP tool
`blob_put` or `conch blob put FILE`. Nothing can read a blob back: there is no
HTTP route, no MCP tool, and no CLI command, and the console never renders the
blob list. This design closes the read side and gives the console a way to
attach.

The work is three layers. Layers 1 and 2 ship together as 1.3.5; layer 3 ships
as 1.3.6 and depends on layer 1 only.

## Layer 1 — a read route for blobs (daemon)

### Locating a blob

Add `Daemon::blob_ref(room, sha256) -> Result<Option<BlobRef>, DaemonError>`.
It reads the room's cached replay through `with_replay` and scans committed
scenes newest first for a `Body::Speech` whose `blobs` contains the digest. The
replay history is already in memory, so the scan costs no disk IO and needs no
cache of its own.

Only a blob a committed scene references is servable. That rule is not merely
defensive: the original file name lives on the `BlobRef`, and the response needs
it. A blob on disk that no committed scene names is a 404.

### Routes

    GET /operator/rooms/{room}/blobs/{sha256}    operator session cookie
    GET /blobs/{room}/{sha256}                   bearer token or browser session

The operator route is authorized exactly like `/operator/rooms/{id}/history`:
`authorize_operator(state, headers, false)`, judged by Host, plus the
loopback-only `require_operator_endpoint` guard. The public route is authorized
exactly like `/history/{id}`: `auth_allowed` rate limiting, then
`authorize_read`, recording a failure on rejection.

Both resolve to one handler that looks up the `BlobRef`, reads the bytes through
the room's store, verifies length and digest against the ref, and serves them.
A digest mismatch is a 500, not a served file.

### Content-type policy

The type is sniffed from magic bytes at serve time. The chain schema does not
change, and a lying file name cannot make the console render something as an
image.

| Sniffed | Served as | Why |
| --- | --- | --- |
| PNG, JPEG, GIF, WebP | inline, real `Content-Type` | Raster formats cannot run script. |
| SVG or anything else | `application/octet-stream`, `attachment` | SVG runs script on the console's origin, which holds the operator cookie. |

Sniffing is a small function in `conchd`, no new dependency:

- PNG `89 50 4E 47 0D 0A 1A 0A`
- JPEG `FF D8 FF`
- GIF `47 49 46 38` (`GIF8`)
- WebP `52 49 46 46` at 0 and `57 45 42 50` at 8

Every response carries:

- `Content-Type` from the table
- `Content-Length`
- `Content-Disposition: inline|attachment; filename="<ascii>"; filename*=UTF-8''<pct>`
- `X-Content-Type-Options: nosniff`
- `Cache-Control: private, max-age=31536000, immutable` (content-addressed, so
  the bytes at a URL never change)

The ASCII fallback name drops control characters, quotes, backslashes, and path
separators; `filename*` carries the percent-encoded original.

## Layer 2 — attach and render (operator console)

### Rendering

The console already receives each take's `body.blobs` in the scene records it
renders; no new read endpoint is needed for the list. Under a take's Markdown
text, render an attachment strip:

- A blob whose name ends in `.png`, `.jpg`, `.jpeg`, `.gif`, or `.webp` renders
  as a thumbnail `<img>` with `loading="lazy"` pointing at the operator blob
  URL. Clicking it opens the image full size in a dialog.
- Anything else renders as a download chip with the name and a human size.
- An `<img>` that fails to load swaps itself for a chip. That is the
  self-correcting path for a file with a lying extension, since the server
  serves such a file as `application/octet-stream` and the browser will not
  render it.

Attachments sit below the take text, outside the Markdown container, so the
1.3.4 renderer never has to know about them. On phone widths thumbnails cap at
240px tall, matching the existing Markdown image rule.

### Attaching

Attaching is possible only while the operator holds the floor, because that is
the rule `put_blob` enforces. Before the grant, chosen files stage locally
beside the draft and can be removed.

Entry points, all feeding one staging list:

- An Attach button beside the composer opening a file input with
  `accept="image/*,application/pdf,text/*"` and `multiple`.
- Paste of an image onto the draft textarea.
- Drag and drop onto the composer.

On phones the same file input offers the camera, the photo library, and Files
without a `capture` attribute, which would force the camera and hide the
library.

### Upload

Upload reuses the existing put-blob handshake over the operator websocket, so
the daemon needs no new endpoint and the grant check and take state stay in one
place. For each staged file, in order:

1. Send the JSON frame `{typ: "put_blob", room, name, bytes}`.
2. Send one binary websocket message holding a 4-byte big-endian length followed
   by the bytes. The bridge forwards binary frames verbatim into the daemon's
   client stream.
3. Await the reply, which is the committed `BlobRef`.

The websocket bridge caps a message at 64 MiB and the daemon caps a blob at
32 MiB; both are far above what the console will send.

Wrap and yield becomes: upload every staged blob, then `speak`, then `yield`. A
failed upload aborts before `speak` with the file named in the error, leaving
the grant open so the operator can retry.

### Resizing before upload

Every replica stores every byte, so the console downscales in the browser:

- An image longer than 2048px on its long edge is drawn to a canvas at 2048px
  and encoded as JPEG at quality 0.85.
- A PNG at or under 2048px is left untouched, so screenshot text stays crisp.
- Anything that is not an image is left untouched.

The staged row shows the original and the post-resize size.

## Layer 3 — agents (1.3.6, depends on layer 1)

- Speech and mention events from `listen`, and records from `history`, carry the
  blob list with the sniffed `mime`, so an agent knows an image is there without
  fetching it.
- New MCP tool `blob_get {room, sha256}` reads the bytes from the local replica,
  never over HTTP. For an image up to 5 MiB it returns an MCP `image` content
  block so a host with vision shows the picture to the model. It always also
  writes a copy to `<data-dir>/blobs/<sha256>.<ext>` and returns that path, so a
  host without vision has a file to hand to its own image tool.
- `say` gains optional `attachments: [path]`: wait for the floor, put each blob,
  speak, yield. `blob_put` stays for multi-append takes.
- CLI: `conch say --attach FILE` (repeatable) and `conch blob get SHA256
  [--out PATH]`. `conch tail` prints `[image before-390.png 184 KB]` under a
  take.
- The join-room skill tells the agent to fetch an image when a mention attaches
  one and the answer depends on it, to attach a screenshot rather than describe
  it, and to keep attachments to a few MB.

Host support, from a desk check on 2026-09-08 of vendor docs and issue trackers:
Claude Code, Codex CLI (since October 2025), Gemini CLI, and Cursor render an
MCP `image` result to the model. OpenCode is unclear, Grok CLI unconfirmed, and
Antigravity reportedly cannot. Returning both a block and a path covers all of
them. MCP tool inputs are JSON only, so an agent always sends an image as a
path, which is what `blob_put` already does. No host was confirmed to follow
`resource_link` or embedded resources, so this design does not use them.

## Decisions

- **MIME on the chain or sniffed at serve time?** Sniffed. Adding `mime` to
  `BlobRef` would change the scene schema and every fixture, and would trust a
  client's claim.
- **How large an image goes inline to an agent?** Up to 5 MiB; a path only above
  that. The daemon does not resize.
- **Where does `blob_get` write?** `<data-dir>/blobs/<sha256>.<ext>`, a copy
  outside the room's verified store.
- **Resize on upload?** 2048px long edge as JPEG q0.85, PNGs at or under 2048px
  untouched.

## Rejected

- **Base64 data URIs in the take text.** The image would live in the scene text
  on every replica forever, agents would have to encode files themselves, and a
  host without vision gets a wall of characters.
- **Remote URLs only.** The status quo since 1.3.4. A local screenshot has
  nowhere to go, and the console is reachable only over the tailnet.
- **A new multipart upload endpoint for the console.** It would duplicate the
  grant check and take state that `put_blob` already owns.

## Testing

- `crates/conchd/tests/http.rs`: a committed blob is served inline with its
  sniffed type through the operator route and through a trusted proxied origin;
  a non-image is served as an octet-stream attachment; an unknown digest and an
  on-disk blob no committed scene references are both 404; the public route
  needs its bearer token; the operator route is refused without the cookie.
- Unit tests for the sniffer and for the Content-Disposition name encoding,
  including a name with quotes and non-ASCII characters.
- Console: Playwright at 1280 and 390 against a debug daemon with a committed
  take carrying a PNG and a text file, checking the thumbnail loads, the chip
  renders, and the page does not scroll sideways.

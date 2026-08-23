const HUMAN = "human:operator";

const state = {
  socket: null,
  pending: [],
  room: localStorage.getItem("conch.room") || new URLSearchParams(location.search).get("room"),
  token: localStorage.getItem("conch.token") || "",
  node: null,
  history: [],
  liveGrant: null,
  refreshing: false,
};

const el = {
  connectionDot: document.getElementById("connection-dot"),
  connectionLabel: document.getElementById("connection-label"),
  roomName: document.getElementById("room-name"),
  roomId: document.getElementById("room-id"),
  nodeId: document.getElementById("node-id"),
  headNumber: document.getElementById("head-number"),
  ticketSource: document.getElementById("ticket-source"),
  joinRole: document.getElementById("join-role"),
  joinButton: document.getElementById("join-button"),
  joinError: document.getElementById("join-error"),
  floorStatus: document.getElementById("floor-status"),
  takeButton: document.getElementById("take-button"),
  transcript: document.getElementById("transcript"),
  draftPreview: document.getElementById("draft-preview"),
  draftText: document.getElementById("draft-text"),
  speech: document.getElementById("speech"),
  composeHint: document.getElementById("compose-hint"),
  yieldButton: document.getElementById("yield-button"),
  wrapButton: document.getElementById("wrap-button"),
};

function wsUrl() {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const query = new URLSearchParams();
  if (state.room) query.set("room", state.room);
  if (state.token) query.set("token", state.token);
  return `${scheme}//${location.host}/client${query.size ? `?${query}` : ""}`;
}

async function connect() {
  if (state.socket) state.socket.close();
  setConnection("connecting", "Connecting");
  const socket = new WebSocket(wsUrl());
  state.socket = socket;
  socket.addEventListener("message", ({ data }) => {
    let message;
    try { message = JSON.parse(data); } catch { return; }
    if (message.typ === "draft") return showRemoteDraft(message);
    const waiter = state.pending.shift();
    if (waiter) message.ok ? waiter.resolve(message.data) : waiter.reject(new Error(message.error?.message || "Request failed"));
  });
  socket.addEventListener("close", () => {
    setConnection("offline", "Disconnected");
    while (state.pending.length) state.pending.shift().reject(new Error("Connection closed"));
  });
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", () => reject(new Error("WebSocket connection failed")), { once: true });
  });
  const attached = await rpc({ typ: "attach", agent: HUMAN });
  state.node = attached.node || null;
  setConnection("online", "Verified connection");
  if (state.room) await refresh();
  renderFacts();
}

function rpc(payload) {
  return new Promise((resolve, reject) => {
    if (!state.socket || state.socket.readyState !== WebSocket.OPEN) return reject(new Error("Not connected"));
    state.pending.push({ resolve, reject });
    state.socket.send(JSON.stringify(payload));
  });
}

async function refresh() {
  if (!state.room || state.refreshing || state.pending.length) return;
  state.refreshing = true;
  try {
    const page = await rpc({ typ: "history", room: state.room, from_n: 0 });
    state.history = page.scenes;
    if (page.syncing) setConnection("syncing", "Showing a verified prefix while catching up");
    await renderHistory();
  } catch (error) {
    setConnection("offline", error.message);
  } finally {
    state.refreshing = false;
  }
}

async function renderHistory() {
  el.transcript.replaceChildren();
  state.liveGrant = null;
  if (!state.history.length) {
    el.transcript.innerHTML = `<div class="empty-state"><span>◎</span><h2>The ledger is quiet</h2><p>No wrapped scenes yet.</p></div>`;
    updateFloor();
    return;
  }
  for (const record of state.history) {
    const { scene, commit_proof: proof } = record;
    const hash = await sceneHash(scene);
    const body = scene.body;
    if (body.closes_grant && state.liveGrant?.hash === body.closes_grant) state.liveGrant = null;
    if (body.type === "grant") state.liveGrant = { hash, to: body.to };

    const fragment = document.getElementById("scene-template").content.cloneNode(true);
    const article = fragment.querySelector("article");
    const title = fragment.querySelector("strong");
    const copy = fragment.querySelector(".scene-content > p");
    article.classList.add(body.type === "speech" ? "speech" : body.type === "grant" ? "grant" : "system");
    fragment.querySelector(".scene-index").textContent = scene.n;
    const rendered = describe(body);
    title.textContent = rendered.title;
    copy.textContent = rendered.copy;
    fragment.querySelector("time").textContent = new Date(scene.ts * 1000).toLocaleString();
    fragment.querySelector("footer").textContent = `${short(hash)} · term ${proof.rpc_term} · ${proof.certs.length} cert${proof.certs.length === 1 ? "" : "s"}`;
    el.transcript.append(fragment);
  }
  renderFacts();
  updateFloor();
  el.transcript.lastElementChild?.scrollIntoView({ block: "nearest" });
}

function describe(body) {
  switch (body.type) {
    case "genesis": return { title: body.name, copy: "Room opened and its genesis wrapped." };
    case "grant": return { title: `${body.to.agent} holds Conch`, copy: `Floor granted on node ${short(body.to.node)}.` };
    case "speech": return { title: "Wrapped take", copy: body.text };
    case "breakout": return { title: "Breakout room", copy: `A child room was opened for ${body.auto_join.length} node(s).` };
    case "membership": return { title: "Room configuration", copy: `Floor mode is now ${body.floor.mode}.` };
    case "view-change": return { title: "Roster changed", copy: `${body.add.length ? "Added" : "Removed"} ${short((body.add[0] || body.remove[0]))}.` };
    default: return { title: body.type, copy: "Wrapped system scene." };
  }
}

function updateFloor() {
  const mine = state.liveGrant?.to.agent === HUMAN && state.liveGrant?.to.node === state.node;
  if (!state.room) el.floorStatus.textContent = "Open a room to see who holds Conch.";
  else if (!state.liveGrant) el.floorStatus.textContent = "The floor is vacant.";
  else if (mine) el.floorStatus.textContent = "You hold Conch. Your next take can be wrapped.";
  else el.floorStatus.textContent = `${state.liveGrant.to.agent} holds Conch on ${short(state.liveGrant.to.node)}.`;
  el.takeButton.disabled = !state.room || Boolean(state.liveGrant);
  el.speech.disabled = !mine;
  el.wrapButton.disabled = !mine || !el.speech.value.trim();
  el.yieldButton.disabled = !mine;
  el.composeHint.textContent = mine ? "Drafts remain unverified until you wrap and yield." : "The wrapped grant controls this composer.";
  if (!mine) hideDraft();
}

function renderFacts() {
  el.roomId.textContent = state.room || "—";
  el.nodeId.textContent = state.node || "—";
  el.headNumber.textContent = state.history.at(-1)?.scene?.n ?? "—";
  const genesis = state.history.find(({ scene }) => scene.n === 0);
  el.roomName.textContent = genesis?.scene?.body?.name || (state.room ? "Conch room" : "No room open");
}

async function joinRoom() {
  el.joinError.textContent = "";
  el.joinButton.disabled = true;
  try {
    const ticket = await parseTicket(el.ticketSource.value.trim());
    const reply = await rpc({ typ: "join", ticket, role: el.joinRole.value });
    state.room = reply.id;
    state.token = ticket.token || "";
    localStorage.setItem("conch.room", state.room);
    if (state.token) localStorage.setItem("conch.token", state.token); else localStorage.removeItem("conch.token");
    history.replaceState(null, "", `?room=${encodeURIComponent(state.room)}`);
    await connect();
    el.ticketSource.value = "";
  } catch (error) {
    el.joinError.textContent = error.message;
  } finally {
    el.joinButton.disabled = false;
  }
}

async function parseTicket(source) {
  if (!source) throw new Error("Paste a ticket, magnet, or URL.");
  if (source.startsWith("{")) return JSON.parse(source);
  if (source.startsWith("http://") || source.startsWith("https://")) {
    const response = await fetch(source);
    if (!response.ok) throw new Error(`Ticket fetch failed (${response.status}).`);
    return response.json();
  }
  if (source.startsWith("conch:1:")) {
    const [identity, query = ""] = source.slice("conch:1:".length).split("?");
    const params = new URLSearchParams(query);
    const genesis = params.get("g");
    if (!genesis) throw new Error("Magnet is missing its g genesis pin.");
    return {
      v: 1, id: identity, name: params.get("dn") || identity,
      trackers: params.getAll("tr"), peers: params.getAll("x.peer"),
      ...(params.get("token") ? { token: params.get("token") } : {}),
      stake: { agents: true, explicit: true, allowlist: [] },
      floor: { mode: "stick", timeout_secs: 30 }, genesis,
    };
  }
  throw new Error("Paste ticket JSON, a conch:1: magnet, or an HTTP URL.");
}

async function takeFloor() {
  el.takeButton.disabled = true;
  try { await rpc({ typ: "raise_hand", room: state.room }); await refresh(); }
  catch (error) { el.floorStatus.textContent = error.message; }
}

async function wrapAndYield() {
  const text = el.speech.value.trim();
  if (!text) return;
  setComposerBusy(true);
  try {
    await rpc({ typ: "speak", room: state.room, text, request_id: crypto.randomUUID().replaceAll("-", "") });
    await rpc({ typ: "yield", room: state.room });
    el.speech.value = "";
    hideDraft();
    await refresh();
  } catch (error) { el.composeHint.textContent = error.message; }
  finally { setComposerBusy(false); updateFloor(); }
}

async function yieldFloor() {
  setComposerBusy(true);
  try { await rpc({ typ: "yield", room: state.room }); await refresh(); }
  catch (error) { el.composeHint.textContent = error.message; }
  finally { setComposerBusy(false); updateFloor(); }
}

function setComposerBusy(busy) { el.speech.disabled = busy; el.wrapButton.disabled = busy; el.yieldButton.disabled = busy; }
function showRemoteDraft(message) { if (message.room === state.room) { el.draftText.textContent = message.text; el.draftPreview.hidden = false; } }
function hideDraft() { el.draftPreview.hidden = true; el.draftText.textContent = ""; }
function setConnection(mode, label) { el.connectionDot.parentElement.className = `connection ${mode}`; el.connectionLabel.textContent = label; }
function short(value) { return value ? `${value.slice(0, 8)}…${value.slice(-5)}` : "—"; }

function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value && typeof value === "object") return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`).join(",")}}`;
  return JSON.stringify(value);
}

async function sceneHash(scene) {
  const unsigned = structuredClone(scene);
  delete unsigned.certs;
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical(unsigned)));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

el.joinButton.addEventListener("click", joinRoom);
el.takeButton.addEventListener("click", takeFloor);
el.wrapButton.addEventListener("click", wrapAndYield);
el.yieldButton.addEventListener("click", yieldFloor);
el.speech.addEventListener("input", () => {
  const text = el.speech.value;
  el.wrapButton.disabled = !text.trim();
  el.draftText.textContent = text;
  el.draftPreview.hidden = !text;
});

connect().catch((error) => setConnection("offline", error.message));
setInterval(refresh, 1500);

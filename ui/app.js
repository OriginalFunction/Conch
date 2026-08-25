const OPERATOR_AGENT = "human:operator";
const POLL_INTERVAL_MS = 2000;
const CATALOG_INTERVAL_MS = 5000;
const BOTTOM_THRESHOLD_PX = 48;
const RETRY_INITIAL_MS = 2000;
const RETRY_MAX_MS = 30000;

const state = {
  operator: false,
  room: roomFromPath(),
  rooms: [],
  detail: null,
  history: [],
  nextN: 0,
  liveGrant: null,
  node: null,
  socket: null,
  pending: [],
  polling: false,
  readOnly: true,
  newMessages: 0,
  invitation: null,
  epoch: 0,
  roomStatus: null,
  retryAt: 0,
  retryDelay: RETRY_INITIAL_MS,
  composerBusy: false,
  floorHolderKey: null,
  queuedIntent: null,
  seenHeads: new Map(),
};

const el = Object.fromEntries([
  "connection", "connection-label", "rooms-toggle", "people-toggle", "rooms-rail", "people-rail",
  "room-count", "room-list", "rooms-empty", "local-node", "home-view", "room-view", "room-name",
  "room-search",
  "room-role", "room-id", "copy-room-id", "head-number", "floor-mode", "floor-status", "take-button",
  "transcript", "scene-list", "history-empty", "new-messages", "new-message-count", "draft-preview",
  "draft-text", "speech", "compose-hint", "yield-button", "wrap-button", "people-count", "people-list",
  "people-empty", "create-room-button", "join-room-button", "home-create-button", "home-join-button",
  "create-dialog", "create-form", "create-name", "create-timeout", "create-error", "join-dialog", "join-form",
  "ticket-source", "join-error", "ticket-dialog", "download-ticket", "copy-magnet", "finish-ticket", "toast",
  "room-state", "room-state-title", "room-state-copy", "reauthorize-room", "room-state-back",
].map((id) => [camel(id), document.getElementById(id)]));

class RoomHttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
    this.roomStatus = status === 401 || status === 403 ? "locked" : status === 404 ? "missing" : null;
  }
}

function camel(value) {
  return value.replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
}

function roomFromPath() {
  const match = location.pathname.match(/^\/rooms\/([0-9a-f]{64})\/?$/i);
  return match ? match[1].toLowerCase() : null;
}

function operatorUrl(path = "") {
  return `/operator${path}`;
}

async function boot() {
  bindEvents();
  setConnection("connecting", "Starting local console");
  state.operator = await bootstrapOperator();
  if (state.operator) {
    await loadCatalog();
    setConnection("online", "Local console ready");
  } else {
    setConnection("offline", state.room ? "Opening room session" : "Room session required");
    el.createRoomButton.hidden = true;
    el.homeCreateButton.hidden = true;
  }
  if (state.room) await openRoom(state.room, false);
  else showHome(false);
  window.setInterval(() => {
    if (state.operator) loadCatalog().catch(reportBackgroundError);
  }, CATALOG_INTERVAL_MS);
  window.setInterval(() => {
    if (state.room) pollRoom();
  }, POLL_INTERVAL_MS);
}

async function bootstrapOperator() {
  try {
    const response = await fetch(operatorUrl("/session"), {
      method: "POST",
      credentials: "same-origin",
      cache: "no-store",
    });
    return response.status === 201;
  } catch {
    return false;
  }
}

async function operatorFetch(path, options = {}, retry = true) {
  const response = await fetch(operatorUrl(path), {
    credentials: "same-origin",
    cache: "no-store",
    ...options,
    headers: options.body ? { "Content-Type": "application/json", ...(options.headers || {}) } : options.headers,
  });
  if (response.status === 403 && retry && await bootstrapOperator()) {
    state.operator = true;
    return operatorFetch(path, options, false);
  }
  return response;
}

async function loadCatalog() {
  const response = await operatorFetch("/rooms");
  if (!response.ok) throw new Error(`Room catalog unavailable (${response.status}).`);
  const catalog = await response.json();
  state.rooms = catalog.rooms || [];
  for (const room of state.rooms) {
    if (!state.seenHeads.has(room.id)) state.seenHeads.set(room.id, room.head_n ?? -1);
  }
  state.node = catalog.node || state.node;
  el.localNode.textContent = short(state.node);
  renderRooms();
}

function renderRooms() {
  const query = el.roomSearch.value.trim().toLowerCase();
  const rooms = state.rooms.filter((room) => !query || room.name.toLowerCase().includes(query) || room.id.includes(query));
  const existing = new Map([...el.roomList.querySelectorAll("[data-room]")].map((link) => [link.dataset.room, link]));
  const visible = new Set();
  let cursor = el.roomList.firstElementChild;
  el.roomCount.textContent = String(state.rooms.length);
  el.roomsEmpty.hidden = rooms.length > 0;
  el.roomsEmpty.querySelector("strong").textContent = state.rooms.length ? "No matching rooms" : "No rooms yet";
  el.roomsEmpty.querySelector("p").textContent = state.rooms.length ? "Try a different name or room id." : "Create one here or join with a ticket.";
  for (const room of rooms) {
    visible.add(room.id);
    let link = existing.get(room.id);
    if (!link) link = document.getElementById("room-item-template").content.firstElementChild.cloneNode(true);
    link.href = `/rooms/${room.id}`;
    link.dataset.room = room.id;
    link.classList.toggle("active", room.id === state.room);
    link.setAttribute("aria-current", room.id === state.room ? "page" : "false");
    link.querySelector("strong").textContent = room.name;
    link.querySelector(".room-preview").textContent = room.floor?.state === "held"
      ? `${room.floor.agent} holds the floor`
      : `Head ${room.head_n ?? "—"} · ${room.roster_size} staker${room.roster_size === 1 ? "" : "s"}`;
    link.querySelector("time").textContent = relativeTime(room.last_activity);
    link.querySelector(".role-dot").classList.toggle("observe", room.role === "observe");
    const unread = room.id === state.room ? 0 : Math.max(0, (room.head_n ?? -1) - (state.seenHeads.get(room.id) ?? -1));
    const badge = link.querySelector(".unread-badge");
    badge.hidden = unread === 0;
    badge.textContent = unread > 99 ? "99+" : String(unread);
    badge.setAttribute("aria-label", `${unread} unread committed message${unread === 1 ? "" : "s"}`);
    if (link !== cursor) el.roomList.insertBefore(link, cursor);
    cursor = link.nextElementSibling;
  }
  for (const [id, link] of existing) if (!visible.has(id)) link.remove();
}

async function openRoom(room, push = true) {
  room = room.toLowerCase();
  if (!/^[0-9a-f]{64}$/i.test(room)) {
    showToast("That room id is invalid.");
    return;
  }
  if (room === state.room && state.roomStatus === "ok") {
    closeMobileRails();
    return;
  }
  if (push) history.pushState({}, "", `/rooms/${room}`);
  closeMobileRails();
  const epoch = resetRoom(room);
  el.homeView.hidden = true;
  el.roomView.hidden = false;
  renderRooms();
  setConnection("connecting", "Opening room");
  try {
    await Promise.all([loadRoomDetail(room, epoch), loadHistory(true, room, epoch)]);
    if (!isCurrentRoom(room, epoch)) return;
    setRoomStatus("ok");
    await connectSocket(room, epoch);
    if (!isCurrentRoom(room, epoch)) return;
    resetReconnect();
    setConnection(state.detail?.room?.syncing ? "connecting" : "online", state.detail?.room?.syncing ? "Catching up" : state.readOnly ? "Verified history · read only" : "Live and verified");
  } catch (error) {
    handleRoomFailure(error, room, epoch);
  }
}

function resetRoom(room) {
  const epoch = ++state.epoch;
  state.socket?.close();
  state.socket = null;
  rejectPending("Room changed");
  state.room = room;
  state.detail = null;
  state.history = [];
  state.nextN = 0;
  state.liveGrant = null;
  state.readOnly = true;
  state.newMessages = 0;
  state.polling = false;
  state.roomStatus = "opening";
  state.floorHolderKey = null;
  state.queuedIntent = null;
  state.composerBusy = false;
  resetReconnect();
  el.speech.value = "";
  hideDraft();
  el.sceneList.replaceChildren();
  el.historyEmpty.hidden = false;
  el.newMessages.hidden = true;
  el.peopleList.replaceChildren();
  el.peopleEmpty.hidden = false;
  el.roomName.textContent = "Opening room…";
  el.roomId.textContent = room;
  el.headNumber.textContent = "—";
  setRoomStatus("opening");
  updateFloor();
  return epoch;
}

function showHome(push = true) {
  if (push) history.pushState({}, "", "/");
  state.socket?.close();
  state.socket = null;
  rejectPending("Room closed");
  state.epoch += 1;
  state.room = null;
  state.roomStatus = null;
  state.detail = null;
  state.history = [];
  state.liveGrant = null;
  state.queuedIntent = null;
  el.homeView.hidden = false;
  el.roomView.hidden = true;
  renderRooms();
  renderPeople();
  closeMobileRails();
  setConnection(state.operator ? "online" : "offline", state.operator ? "Local console ready" : "Open a room");
}

async function loadRoomDetail(room = state.room, epoch = state.epoch) {
  const response = state.operator
    ? await operatorFetch(`/rooms/${room}`)
    : await fetch(`/room/${room}`, { credentials: "same-origin", cache: "no-store" });
  if (!response.ok) throw roomResponseError(response, "Room details unavailable");
  const detail = await response.json();
  if (!isCurrentRoom(room, epoch)) return;
  state.detail = detail;
  state.node = state.detail.node || state.node;
  renderRoomDetail();
}

function renderRoomDetail() {
  const room = state.detail?.room;
  if (!room) return;
  el.roomName.textContent = room.name;
  document.title = `${room.name} · Conch`;
  el.roomId.textContent = room.id;
  el.roomRole.textContent = room.role;
  el.roomRole.className = `role-badge ${room.role}`;
  el.headNumber.textContent = room.head_n ?? "—";
  el.floorMode.textContent = state.detail.floor?.mode || "—";
  el.localNode.textContent = short(state.node);
  const previousHolder = state.floorHolderKey;
  const holder = state.detail.floor?.holder || null;
  const nextHolder = holderKey(holder);
  state.liveGrant = holder
    ? { to: holder, hash: null }
    : null;
  state.queuedIntent = (state.detail.floor?.queue || []).find((intent) =>
    intent.agent === OPERATOR_AGENT && intent.node === state.node
  ) || null;
  state.floorHolderKey = nextHolder;
  if (previousHolder !== null && previousHolder !== nextHolder) hideDraft();
  renderPeople();
  updateFloor();
}

function renderPeople() {
  const mouths = Array.isArray(state.detail?.mouths)
    ? state.detail.mouths
    : (state.detail?.participants || []).flatMap((participant) =>
        (participant.agents || []).map((agent) => ({ ...participant, agent }))
      );
  const existing = new Map([...el.peopleList.querySelectorAll("[data-person]")].map((article) => [article.dataset.person, article]));
  const visible = new Set();
  let cursor = el.peopleList.firstElementChild;
  el.peopleCount.textContent = String(mouths.length);
  el.peopleEmpty.hidden = mouths.length > 0;
  el.peopleEmpty.querySelector("strong").textContent = state.room ? "No people observed" : "No room open";
  el.peopleEmpty.querySelector("p").textContent = state.room ? "Agent mouths appear after they participate in this room." : "Choose a room to see people and their roles.";
  for (const mouth of mouths) {
    const key = `${mouth.node}:${mouth.agent}`;
    visible.add(key);
    let article = existing.get(key);
    if (!article) article = document.getElementById("person-template").content.firstElementChild.cloneNode(true);
    article.dataset.person = key;
    article.dataset.node = mouth.node;
    article.classList.toggle("recent", mouth.recent);
    article.querySelector(".person-avatar span").textContent = initials(mouth.agent);
    article.querySelector(".person-main > strong").textContent = mouth.agent;
    article.querySelector(".agent-empty").hidden = true;
    article.querySelector("code").textContent = mouth.node;
    const badges = article.querySelector(".badges");
    badges.replaceChildren();
    badges.append(badge(mouth.role, mouth.role));
    if (mouth.local) badges.append(badge("local", "this node"));
    if (mouth.leader) badges.append(badge("leader", "leader node"));
    if (mouth.floor_holder) badges.append(badge("floor", "holds floor"));
    if (mouth.moderator) badges.append(badge("moderator", "moderator"));
    if (article !== cursor) el.peopleList.insertBefore(article, cursor);
    cursor = article.nextElementSibling;
  }
  for (const [key, article] of existing) if (!visible.has(key)) article.remove();
}

function badge(kind, label) {
  const span = document.createElement("span");
  span.className = `badge ${kind}`;
  span.textContent = label;
  return span;
}

async function loadHistory(initial = false, room = state.room, epoch = state.epoch) {
  const from = initial ? 0 : state.nextN;
  const response = state.operator
    ? await operatorFetch(`/rooms/${room}/history?from=${from}`)
    : await fetch(`/history/${room}?from=${from}`, { credentials: "same-origin", cache: "no-store" });
  if (!response.ok) throw roomResponseError(response, "History unavailable");
  const page = await response.json();
  if (!isCurrentRoom(room, epoch)) return;
  const incoming = page.scenes || [];
  if (incoming.length) await appendHistory(incoming, initial, room, epoch);
  if (!isCurrentRoom(room, epoch)) return;
  if (page.syncing) setConnection("connecting", "Showing verified history while catching up");
}

async function appendHistory(records, initial, room = state.room, epoch = state.epoch) {
  const known = new Set(state.history.map(({ scene }) => scene.n));
  const fresh = records.filter(({ scene }) => !known.has(scene.n)).sort((a, b) => a.scene.n - b.scene.n);
  if (!fresh.length) return;
  const stickToBottom = initial || isNearBottom();
  const rendered = await Promise.all(fresh.map(renderScene));
  if (!isCurrentRoom(room, epoch)) return;
  for (let index = 0; index < fresh.length; index += 1) {
    const record = fresh[index];
    state.history.push(record);
    state.nextN = Math.max(state.nextN, record.scene.n + 1);
    await updateGrantFromRecord(record);
    if (!isCurrentRoom(room, epoch)) return;
    el.sceneList.append(rendered[index]);
  }
  el.historyEmpty.hidden = state.history.length > 0;
  el.headNumber.textContent = state.history.at(-1)?.scene.n ?? "—";
  state.seenHeads.set(room, state.history.at(-1)?.scene.n ?? -1);
  renderRooms();
  updateFloor();
  if (stickToBottom) {
    requestAnimationFrame(scrollToLatest);
  } else {
    state.newMessages += fresh.length;
    renderNewMessages();
  }
}

async function updateGrantFromRecord(record) {
  const body = record.scene.body;
  if (body.closes_grant && state.liveGrant?.hash === body.closes_grant) {
    state.liveGrant = null;
    state.floorHolderKey = null;
    hideDraft();
  }
  if (body.type === "grant") {
    const nextHolder = holderKey(body.to);
    if (state.floorHolderKey !== null && state.floorHolderKey !== nextHolder) hideDraft();
    state.floorHolderKey = nextHolder;
    state.liveGrant = { hash: null, to: body.to };
    const hash = await sceneHash(record.scene);
    if (state.liveGrant?.to.node === body.to.node && state.liveGrant?.to.agent === body.to.agent) state.liveGrant.hash = hash;
  }
}

async function renderScene(record) {
  const { scene, commit_proof: proof } = record;
  const fragment = document.getElementById("scene-template").content.cloneNode(true);
  const article = fragment.querySelector("article");
  article.dataset.n = String(scene.n);
  const body = scene.body;
  article.classList.add(body.type === "speech" ? "speech" : body.type === "grant" ? "grant" : "system");
  fragment.querySelector(".scene-marker span").textContent = scene.n;
  const rendered = describe(body);
  fragment.querySelector("strong").textContent = rendered.title;
  fragment.querySelector(".scene-kind").textContent = rendered.kind;
  fragment.querySelector(".scene-content > p").textContent = rendered.copy;
  const time = fragment.querySelector("time");
  time.dateTime = new Date(scene.ts * 1000).toISOString();
  time.textContent = formatTimestamp(scene.ts);
  const hash = await sceneHash(scene);
  fragment.querySelector("footer").textContent = `${short(hash)} · term ${proof.rpc_term} · ${proof.certs.length} cert${proof.certs.length === 1 ? "" : "s"}`;
  return fragment;
}

function describe(body) {
  switch (body.type) {
    case "genesis": return { title: body.name, kind: "Genesis", copy: "Room opened and its first scene committed." };
    case "grant": return { title: body.to.agent, kind: "Floor granted", copy: `Now holds Conch on node ${short(body.to.node)}.` };
    case "speech": return { title: "Wrapped take", kind: "Speech", copy: body.text || "Empty take" };
    case "breakout": return { title: "Breakout created", kind: "System", copy: `Opened a child room for ${body.auto_join.length} node(s).` };
    case "membership": return { title: "Room configuration changed", kind: "System", copy: `Floor mode is now ${body.floor.mode}.` };
    case "view-change": return { title: "Roster changed", kind: "System", copy: describeViewChange(body) };
    default: return { title: body.type || "System scene", kind: "System", copy: "Committed system scene." };
  }
}

function describeViewChange(body) {
  if (body.add?.length) return `Added ${body.add.map(short).join(", ")} as a staker.`;
  if (body.remove?.length) return `Removed ${body.remove.map(short).join(", ")} from the roster.`;
  return "Committed a roster view change.";
}

async function pollRoom(force = false) {
  if (!state.room || state.polling || state.roomStatus === "locked" || state.roomStatus === "missing" || Date.now() < state.retryAt) return;
  const room = state.room;
  const epoch = state.epoch;
  state.polling = true;
  try {
    await Promise.all([loadRoomDetail(room, epoch), loadHistory(false, room, epoch)]);
    if (!isCurrentRoom(room, epoch)) return;
    setRoomStatus("ok");
    if (force && state.operator) await loadCatalog();
    if (!state.socket || state.socket.readyState > WebSocket.OPEN) await connectSocket(room, epoch);
    if (!isCurrentRoom(room, epoch)) return;
    resetReconnect();
    setConnection(state.detail?.room?.syncing ? "connecting" : "online", state.detail?.room?.syncing ? "Catching up" : state.readOnly ? "Verified history · read only" : "Live and verified");
  } catch (error) {
    handleRoomFailure(error, room, epoch);
  } finally {
    if (epoch === state.epoch) state.polling = false;
  }
}

async function connectSocket(room = state.room, epoch = state.epoch) {
  if (!isCurrentRoom(room, epoch) || state.socket?.readyState === WebSocket.OPEN) return;
  if (state.detail?.room?.browser_mutable === false) {
    state.readOnly = true;
    updateFloor();
    return;
  }
  state.socket?.close();
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const path = state.operator ? `/operator/client/${room}` : "/client";
  const socket = new WebSocket(`${scheme}//${location.host}${path}`);
  state.socket = socket;
  socket.addEventListener("message", ({ data }) => {
    if (!isCurrentRoom(room, epoch) || state.socket !== socket) return;
    if (typeof data !== "string") return;
    let message;
    try { message = JSON.parse(data); } catch { return; }
    if (message.typ === "draft") return showRemoteDraft(message);
    const waiter = state.pending.shift();
    if (waiter) message.ok ? waiter.resolve(message.data) : waiter.reject(new Error(message.error?.message || "Request failed"));
  });
  socket.addEventListener("close", () => {
    if (!isCurrentRoom(room, epoch) || state.socket !== socket) return;
    state.socket = null;
    state.readOnly = true;
    rejectPending("Connection closed");
    updateFloor();
  });
  await new Promise((resolve, reject) => {
    const timer = window.setTimeout(() => reject(new Error("Live connection timed out.")), 5000);
    socket.addEventListener("open", () => { window.clearTimeout(timer); resolve(); }, { once: true });
    socket.addEventListener("error", () => { window.clearTimeout(timer); reject(new Error("Live connection unavailable.")); }, { once: true });
  });
  const attached = await rpc({ typ: "attach", agent: OPERATOR_AGENT });
  if (!isCurrentRoom(room, epoch) || state.socket !== socket) {
    socket.close();
    return;
  }
  state.node = attached.node || state.node;
  state.readOnly = false;
  el.localNode.textContent = short(state.node);
  updateFloor();
}

function rpc(payload) {
  return new Promise((resolve, reject) => {
    if (!state.socket || state.socket.readyState !== WebSocket.OPEN) return reject(new Error("Live connection is unavailable."));
    state.pending.push({ resolve, reject });
    state.socket.send(JSON.stringify(payload));
  });
}

function rejectPending(message) {
  while (state.pending.length) state.pending.shift().reject(new Error(message));
}

function updateFloor() {
  const holder = state.detail?.floor?.holder || state.liveGrant?.to || null;
  const mine = holder?.agent === OPERATOR_AGENT && holder?.node === state.node;
  const queued = state.queuedIntent;
  if (!state.room) el.floorStatus.textContent = "Open a room to see who holds Conch.";
  else if (!holder && queued) el.floorStatus.textContent = `Your hand is raised${queued.position ? ` at #${queued.position}` : ""}. Waiting for the grant to commit.`;
  else if (!holder) el.floorStatus.textContent = "The floor is vacant. Raise your hand to speak.";
  else if (mine) el.floorStatus.textContent = "You hold Conch. Your next take can be wrapped.";
  else if (queued) el.floorStatus.textContent = `${holder.agent} holds Conch. Your hand is raised${queued.position ? ` at #${queued.position}` : ""}.`;
  else el.floorStatus.textContent = `${holder.agent} holds Conch on ${short(holder.node)}. You can queue now.`;
  el.takeButton.textContent = queued ? `Hand raised${queued.position ? ` · #${queued.position}` : ""}` : "Raise hand";
  el.takeButton.disabled = state.composerBusy || state.readOnly || state.roomStatus !== "ok" || !state.room || mine || Boolean(queued);
  el.speech.disabled = state.composerBusy || !mine;
  el.wrapButton.disabled = state.composerBusy || !mine || !el.speech.value.trim();
  el.yieldButton.disabled = state.composerBusy || !mine;
  el.composeHint.textContent = state.readOnly
    ? state.detail?.room?.browser_mutable === false ? "This tokenless room is available as verified read-only history." : "The committed history is available; live room mutations are reconnecting."
    : mine ? "Your draft is local until you wrap it." : "The committed grant controls this composer.";
}

async function takeFloor() {
  el.takeButton.disabled = true;
  try {
    const queued = await rpc({ typ: "raise_hand", room: state.room });
    state.queuedIntent = { intent_id: queued.intent_id, agent: OPERATOR_AGENT, node: state.node };
    updateFloor();
    await pollRoom(true);
  } catch (error) {
    showToast(error.message);
  } finally {
    updateFloor();
  }
}

async function wrapAndYield() {
  const text = el.speech.value;
  if (!text.trim()) return;
  setComposerBusy(true);
  try {
    await rpc({ typ: "speak", room: state.room, text, request_id: requestId() });
    await rpc({ typ: "yield", room: state.room });
    el.speech.value = "";
    hideDraft();
    await pollRoom(true);
  } catch (error) {
    showToast(error.message);
  } finally {
    setComposerBusy(false);
    updateFloor();
  }
}

async function yieldFloor() {
  setComposerBusy(true);
  try {
    await rpc({ typ: "yield", room: state.room });
    await pollRoom(true);
  } catch (error) {
    showToast(error.message);
  } finally {
    setComposerBusy(false);
    updateFloor();
  }
}

async function createRoom(event) {
  event.preventDefault();
  if (!state.operator) return;
  el.createError.textContent = "";
  const submit = event.submitter;
  submit.disabled = true;
  try {
    const response = await operatorFetch("/rooms", {
      method: "POST",
      body: JSON.stringify({
        name: el.createName.value,
        mode: "stick",
        timeout_secs: Number(el.createTimeout.value),
      }),
    });
    const result = await responseJson(response);
    if (!response.ok) throw new Error(result.error || `Room creation failed (${response.status}).`);
    state.invitation = result.ticket;
    el.createDialog.close();
    el.createForm.reset();
    el.createTimeout.value = "30";
    await loadCatalog();
    await openRoom(result.ticket.id);
    el.ticketDialog.showModal();
  } catch (error) {
    el.createError.textContent = error.message;
  } finally {
    submit.disabled = false;
  }
}

async function joinRoom(event) {
  event.preventDefault();
  el.joinError.textContent = "";
  const submit = event.submitter;
  submit.disabled = true;
  const source = el.ticketSource.value.trim();
  el.ticketSource.value = "";
  try {
    const ticket = await parseTicket(source);
    const role = new FormData(el.joinForm).get("join-role") || "stake";
    if (state.operator) {
      const response = await operatorFetch("/rooms/join", {
        method: "POST",
        body: JSON.stringify({ ticket, role }),
      });
      const result = await responseJson(response);
      if (!response.ok) throw new Error(result.error || `Join failed (${response.status}).`);
      await loadCatalog();
    } else if (ticket.token) {
      const response = await fetch(`/session/${ticket.id}`, {
        method: "POST",
        headers: { Authorization: `Bearer ${ticket.token}` },
        credentials: "same-origin",
        cache: "no-store",
      });
      if (!response.ok) throw new RoomHttpError(response.status, `Session authorization failed (${response.status}).`);
    }
    el.joinDialog.close();
    await openRoom(ticket.id);
  } catch (error) {
    el.joinError.textContent = error.message;
  } finally {
    submit.disabled = false;
  }
}

async function parseTicket(source) {
  if (!source) throw new Error("Paste a ticket, magnet, or URL.");
  if (source.startsWith("{")) return JSON.parse(source);
  if (source.startsWith("http://") || source.startsWith("https://")) {
    const response = await fetch(source, { redirect: "follow", cache: "no-store" });
    if (!response.ok) throw new Error(`Ticket fetch failed (${response.status}).`);
    return responseJson(response);
  }
  if (source.startsWith("conch:1:")) {
    const [identity, query = ""] = source.slice("conch:1:".length).split("?");
    const params = new URLSearchParams(query);
    const genesis = params.get("g");
    if (!genesis) throw new Error("Magnet is missing its g genesis pin.");
    return {
      v: 1,
      id: identity,
      name: params.get("dn") || identity,
      trackers: params.getAll("tr"),
      peers: params.getAll("x.peer"),
      ...(params.get("token") ? { token: params.get("token") } : {}),
      stake: { agents: true, explicit: true, allowlist: [] },
      floor: { mode: "stick", timeout_secs: 30 },
      genesis,
    };
  }
  throw new Error("Paste ticket JSON, a conch:1: magnet, or an HTTP URL.");
}

function ticketMagnet(ticket) {
  const params = new URLSearchParams();
  params.set("dn", ticket.name);
  params.set("g", ticket.genesis);
  for (const tracker of ticket.trackers || []) params.append("tr", tracker);
  for (const peer of ticket.peers || []) params.append("x.peer", peer);
  if (ticket.token) params.set("token", ticket.token);
  return `conch:1:${ticket.id}?${params}`;
}

function downloadInvitation() {
  if (!state.invitation) return;
  const blob = new Blob([`${JSON.stringify(state.invitation, null, 2)}\n`], { type: "application/json" });
  const link = document.createElement("a");
  link.href = URL.createObjectURL(blob);
  link.download = `${slug(state.invitation.name) || "room"}.conch`;
  document.body.append(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(link.href), 10000);
  showToast("Invitation downloaded.");
}

async function copyMagnet() {
  if (!state.invitation) return;
  await copyText(ticketMagnet(state.invitation), "Private magnet copied.");
}

function closeInvitation() {
  state.invitation = null;
  el.ticketDialog.close();
}

function bindEvents() {
  document.querySelector("[data-home]").addEventListener("click", (event) => { event.preventDefault(); showHome(); });
  el.roomList.addEventListener("click", (event) => {
    const link = event.target.closest("[data-room]");
    if (!link) return;
    event.preventDefault();
    if (link.dataset.room !== state.room || state.roomStatus !== "ok") openRoom(link.dataset.room);
  });
  el.roomSearch.addEventListener("input", renderRooms);
  for (const button of [el.createRoomButton, el.homeCreateButton]) button.addEventListener("click", () => el.createDialog.showModal());
  for (const button of [el.joinRoomButton, el.homeJoinButton]) button.addEventListener("click", () => el.joinDialog.showModal());
  document.querySelectorAll("[data-close]").forEach((button) => button.addEventListener("click", () => button.closest("dialog").close()));
  el.createForm.addEventListener("submit", createRoom);
  el.joinForm.addEventListener("submit", joinRoom);
  el.takeButton.addEventListener("click", takeFloor);
  el.wrapButton.addEventListener("click", wrapAndYield);
  el.yieldButton.addEventListener("click", yieldFloor);
  el.speech.addEventListener("input", () => {
    el.draftText.textContent = el.speech.value;
    el.draftPreview.hidden = !el.speech.value;
    updateFloor();
  });
  el.transcript.addEventListener("scroll", () => {
    if (isNearBottom()) { state.newMessages = 0; renderNewMessages(); }
  }, { passive: true });
  el.newMessages.addEventListener("click", scrollToLatest);
  el.copyRoomId.addEventListener("click", async () => {
    if (!state.room) return;
    await copyText(state.room, "Room id copied.");
  });
  el.downloadTicket.addEventListener("click", downloadInvitation);
  el.copyMagnet.addEventListener("click", copyMagnet);
  el.finishTicket.addEventListener("click", closeInvitation);
  el.ticketDialog.addEventListener("cancel", () => { state.invitation = null; });
  el.joinDialog.addEventListener("close", clearJoinSecret);
  el.reauthorizeRoom.addEventListener("click", () => el.joinDialog.showModal());
  el.roomStateBack.addEventListener("click", () => showHome());
  el.roomsToggle.addEventListener("click", () => toggleRail("rooms"));
  el.peopleToggle.addEventListener("click", () => toggleRail("people"));
  window.addEventListener("resize", syncRailAccessibility);
  window.addEventListener("popstate", () => {
    const room = roomFromPath();
    if (room) openRoom(room, false); else showHome(false);
  });
  syncRailAccessibility();
}

function toggleRail(which) {
  const target = which === "rooms" ? el.roomsRail : el.peopleRail;
  const other = which === "rooms" ? el.peopleRail : el.roomsRail;
  const button = which === "rooms" ? el.roomsToggle : el.peopleToggle;
  const otherButton = which === "rooms" ? el.peopleToggle : el.roomsToggle;
  other.classList.remove("open");
  otherButton.setAttribute("aria-expanded", "false");
  const open = target.classList.toggle("open");
  button.setAttribute("aria-expanded", String(open));
  syncRailAccessibility();
}

function closeMobileRails() {
  el.roomsRail.classList.remove("open");
  el.peopleRail.classList.remove("open");
  el.roomsToggle.setAttribute("aria-expanded", "false");
  el.peopleToggle.setAttribute("aria-expanded", "false");
  syncRailAccessibility();
}

function isNearBottom() {
  return el.transcript.scrollHeight - el.transcript.scrollTop - el.transcript.clientHeight <= BOTTOM_THRESHOLD_PX;
}

function scrollToLatest() {
  el.transcript.scrollTop = el.transcript.scrollHeight;
  state.newMessages = 0;
  renderNewMessages();
}

function renderNewMessages() {
  el.newMessages.hidden = state.newMessages === 0;
  el.newMessageCount.textContent = String(state.newMessages);
  el.newMessages.lastChild.textContent = ` new message${state.newMessages === 1 ? "" : "s"}`;
}

function setComposerBusy(busy) {
  state.composerBusy = busy;
  updateFloor();
}

function showRemoteDraft(message) {
  if (message.room !== state.room) return;
  el.draftText.textContent = message.text || "";
  el.draftPreview.hidden = false;
}

function hideDraft() {
  el.draftPreview.hidden = true;
  el.draftText.textContent = "";
}

function setConnection(mode, label) {
  el.connection.className = `connection ${mode}`;
  el.connectionLabel.textContent = label;
}

let toastTimer;
function showToast(message) {
  window.clearTimeout(toastTimer);
  el.toast.textContent = message;
  el.toast.hidden = false;
  toastTimer = window.setTimeout(() => { el.toast.hidden = true; }, 3200);
}

function reportBackgroundError(error) {
  if (state.room) setConnection("offline", error.message);
}

function roomResponseError(response, fallback) {
  if (response.status === 401 || response.status === 403) {
    return new RoomHttpError(response.status, "This room session has expired. Join with its ticket to reauthorize.");
  }
  if (response.status === 404) {
    return new RoomHttpError(response.status, "This room is not stored on the current daemon.");
  }
  return new RoomHttpError(response.status, `${fallback} (${response.status}).`);
}

async function responseJson(response) {
  const text = await response.text();
  if (!text) return {};
  try {
    return JSON.parse(text);
  } catch {
    if (!response.ok) return { error: `Request failed (${response.status}).` };
    throw new Error("The server returned an invalid JSON response.");
  }
}

function isCurrentRoom(room, epoch) {
  return state.room === room && state.epoch === epoch;
}

function holderKey(holder) {
  return holder ? `${holder.node}:${holder.agent}` : null;
}

function resetReconnect() {
  state.retryAt = 0;
  state.retryDelay = RETRY_INITIAL_MS;
}

function handleRoomFailure(error, room, epoch) {
  if (!isCurrentRoom(room, epoch)) return;
  state.readOnly = true;
  state.socket?.close();
  state.socket = null;
  rejectPending("Connection unavailable");
  updateFloor();
  if (error.roomStatus) {
    state.retryAt = Number.POSITIVE_INFINITY;
    setRoomStatus(error.roomStatus, error.message);
    setConnection("offline", error.roomStatus === "locked" ? "Room authorization required" : "Room unavailable");
    return;
  }
  const delay = state.retryDelay;
  state.retryAt = Date.now() + delay;
  state.retryDelay = Math.min(delay * 2, RETRY_MAX_MS);
  setConnection("offline", `${error.message} Retrying in ${Math.ceil(delay / 1000)}s.`);
}

function setRoomStatus(status, message = "") {
  state.roomStatus = status;
  const blocked = status === "locked" || status === "missing";
  el.roomState.hidden = !blocked;
  el.roomView.classList.toggle("room-blocked", blocked);
  if (!blocked) return;
  const locked = status === "locked";
  el.roomStateTitle.textContent = locked ? "Room access expired" : "Room not available";
  el.roomStateCopy.textContent = message || (locked
    ? "Paste the room ticket again to renew this browser's room-scoped session."
    : "This daemon does not have that room. Return to the room library or join it from a ticket.");
  el.reauthorizeRoom.hidden = !locked;
}

async function copyText(value, confirmation) {
  try {
    await navigator.clipboard.writeText(value);
    showToast(confirmation);
  } catch {
    showToast("Clipboard access was denied. Select and copy the value manually.");
  }
}

function clearJoinSecret() {
  el.ticketSource.value = "";
  el.joinError.textContent = "";
}

function syncRailAccessibility() {
  const mobile = window.matchMedia("(max-width: 860px)").matches;
  el.roomsRail.inert = mobile && !el.roomsRail.classList.contains("open");
  el.peopleRail.inert = mobile && !el.peopleRail.classList.contains("open");
}

function requestId() {
  return crypto.randomUUID().replaceAll("-", "");
}

function short(value) {
  return value ? `${value.slice(0, 7)}…${value.slice(-4)}` : "—";
}

function initials(value) {
  const clean = String(value).split(":").at(-1).replace(/[^a-z0-9]+/gi, " ").trim();
  return (clean.split(/\s+/).map((part) => part[0]).join("").slice(0, 2) || "N").toUpperCase();
}

function slug(value) {
  return value.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 64);
}

function formatTimestamp(seconds) {
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(seconds * 1000));
}

function relativeTime(seconds) {
  if (!seconds) return "—";
  const delta = Math.max(0, Math.floor(Date.now() / 1000) - seconds);
  if (delta < 60) return "now";
  if (delta < 3600) return `${Math.floor(delta / 60)}m`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h`;
  return `${Math.floor(delta / 86400)}d`;
}

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

boot().catch((error) => setConnection("offline", error.message));

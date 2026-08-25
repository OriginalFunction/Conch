const OPERATOR_AGENT = "human:operator";
const POLL_INTERVAL_MS = 2000;
const CATALOG_INTERVAL_MS = 5000;
const BOTTOM_THRESHOLD_PX = 48;

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
].map((id) => [camel(id), document.getElementById(id)]));

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
    if (state.room) pollRoom().catch(reportBackgroundError);
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
  state.node = catalog.node || state.node;
  el.localNode.textContent = short(state.node);
  renderRooms();
}

function renderRooms() {
  el.roomList.replaceChildren();
  const query = el.roomSearch.value.trim().toLowerCase();
  const rooms = state.rooms.filter((room) => !query || room.name.toLowerCase().includes(query) || room.id.includes(query));
  el.roomCount.textContent = String(state.rooms.length);
  el.roomsEmpty.hidden = rooms.length > 0;
  el.roomsEmpty.querySelector("strong").textContent = state.rooms.length ? "No matching rooms" : "No rooms yet";
  el.roomsEmpty.querySelector("p").textContent = state.rooms.length ? "Try a different name or room id." : "Create one here or join with a ticket.";
  for (const room of rooms) {
    const fragment = document.getElementById("room-item-template").content.cloneNode(true);
    const link = fragment.querySelector("a");
    link.href = `/rooms/${room.id}`;
    link.dataset.room = room.id;
    link.classList.toggle("active", room.id === state.room);
    link.setAttribute("aria-current", room.id === state.room ? "page" : "false");
    fragment.querySelector("strong").textContent = room.name;
    fragment.querySelector(".room-preview").textContent = room.floor?.state === "held"
      ? `${room.floor.agent} holds the floor`
      : `Head ${room.head_n ?? "—"} · ${room.roster_size} staker${room.roster_size === 1 ? "" : "s"}`;
    fragment.querySelector("time").textContent = relativeTime(room.last_activity);
    fragment.querySelector(".role-dot").classList.toggle("observe", room.role === "observe");
    el.roomList.append(fragment);
  }
}

async function openRoom(room, push = true) {
  if (!/^[0-9a-f]{64}$/i.test(room)) {
    showToast("That room id is invalid.");
    return;
  }
  if (push) history.pushState({}, "", `/rooms/${room}`);
  closeMobileRails();
  resetRoom(room);
  el.homeView.hidden = true;
  el.roomView.hidden = false;
  renderRooms();
  setConnection("connecting", "Opening room");
  try {
    await Promise.all([loadRoomDetail(), loadHistory(true)]);
    await connectSocket();
    setConnection(state.detail?.room?.syncing ? "connecting" : "online", state.detail?.room?.syncing ? "Catching up" : "Live and verified");
  } catch (error) {
    state.readOnly = true;
    updateFloor();
    setConnection("offline", error.message);
  }
}

function resetRoom(room) {
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
  el.sceneList.replaceChildren();
  el.historyEmpty.hidden = false;
  el.newMessages.hidden = true;
  el.peopleList.replaceChildren();
  el.peopleEmpty.hidden = false;
  el.roomName.textContent = "Opening room…";
  el.roomId.textContent = room;
  el.headNumber.textContent = "—";
  updateFloor();
}

function showHome(push = true) {
  if (push) history.pushState({}, "", "/");
  state.socket?.close();
  state.socket = null;
  rejectPending("Room closed");
  state.room = null;
  state.detail = null;
  state.history = [];
  state.liveGrant = null;
  el.homeView.hidden = false;
  el.roomView.hidden = true;
  renderRooms();
  renderPeople();
  closeMobileRails();
  setConnection(state.operator ? "online" : "offline", state.operator ? "Local console ready" : "Open a room");
}

async function loadRoomDetail() {
  const response = state.operator
    ? await operatorFetch(`/rooms/${state.room}`)
    : await fetch(`/room/${state.room}`, { credentials: "same-origin", cache: "no-store" });
  if (!response.ok) throw new Error(response.status === 401 || response.status === 403
    ? "This browser is not authorized for that room. Join with its ticket."
    : `Room details unavailable (${response.status}).`);
  state.detail = await response.json();
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
  state.liveGrant = state.detail.floor?.holder
    ? { to: state.detail.floor.holder, hash: null }
    : null;
  renderPeople();
  updateFloor();
}

function renderPeople() {
  const participants = state.detail?.participants || [];
  el.peopleList.replaceChildren();
  el.peopleCount.textContent = String(participants.length);
  el.peopleEmpty.hidden = participants.length > 0;
  for (const participant of participants) {
    const fragment = document.getElementById("person-template").content.cloneNode(true);
    const article = fragment.querySelector("article");
    article.classList.toggle("recent", participant.recent);
    const agents = participant.agents || [];
    const display = agents.length ? agents.join(", ") : `Node ${short(participant.node)}`;
    fragment.querySelector(".person-avatar span").textContent = initials(agents[0] || participant.node);
    fragment.querySelector(".person-main > strong").textContent = display;
    fragment.querySelector("code").textContent = participant.node;
    const badges = fragment.querySelector(".badges");
    badges.append(badge(participant.role, participant.role));
    if (participant.local) badges.append(badge("local", "this node"));
    if (participant.leader) badges.append(badge("leader", "leader"));
    if (participant.floor_holder) badges.append(badge("floor", "holds floor"));
    if (participant.moderator) badges.append(badge("moderator", "moderator"));
    el.peopleList.append(fragment);
  }
}

function badge(kind, label) {
  const span = document.createElement("span");
  span.className = `badge ${kind}`;
  span.textContent = label;
  return span;
}

async function loadHistory(initial = false) {
  const from = initial ? 0 : state.nextN;
  const response = state.operator
    ? await operatorFetch(`/rooms/${state.room}/history?from=${from}`)
    : await fetch(`/history/${state.room}?from=${from}`, { credentials: "same-origin", cache: "no-store" });
  if (!response.ok) throw new Error(`History unavailable (${response.status}).`);
  const page = await response.json();
  const incoming = page.scenes || [];
  if (incoming.length) await appendHistory(incoming, initial);
  if (page.syncing) setConnection("connecting", "Showing verified history while catching up");
}

async function appendHistory(records, initial) {
  const known = new Set(state.history.map(({ scene }) => scene.n));
  const fresh = records.filter(({ scene }) => !known.has(scene.n)).sort((a, b) => a.scene.n - b.scene.n);
  if (!fresh.length) return;
  const stickToBottom = initial || isNearBottom();
  for (const record of fresh) {
    state.history.push(record);
    state.nextN = Math.max(state.nextN, record.scene.n + 1);
    updateGrantFromRecord(record);
    el.sceneList.append(await renderScene(record));
  }
  el.historyEmpty.hidden = state.history.length > 0;
  el.headNumber.textContent = state.history.at(-1)?.scene.n ?? "—";
  updateFloor();
  if (stickToBottom) {
    requestAnimationFrame(scrollToLatest);
  } else {
    state.newMessages += fresh.length;
    renderNewMessages();
  }
}

function updateGrantFromRecord(record) {
  const body = record.scene.body;
  if (body.closes_grant && state.liveGrant?.hash === body.closes_grant) state.liveGrant = null;
  if (body.type === "grant") {
    state.liveGrant = { hash: null, to: body.to };
    sceneHash(record.scene).then((hash) => {
      if (state.liveGrant?.to.node === body.to.node && state.liveGrant?.to.agent === body.to.agent) state.liveGrant.hash = hash;
    });
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
  if (!state.room || state.polling) return;
  state.polling = true;
  try {
    await Promise.all([loadRoomDetail(), loadHistory(false)]);
    if (force && state.operator) await loadCatalog();
    if (!state.socket || state.socket.readyState > WebSocket.OPEN) await connectSocket();
  } finally {
    state.polling = false;
  }
}

async function connectSocket() {
  if (!state.room || state.socket?.readyState === WebSocket.OPEN) return;
  state.socket?.close();
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const path = state.operator ? `/operator/client/${state.room}` : "/client";
  const socket = new WebSocket(`${scheme}//${location.host}${path}`);
  state.socket = socket;
  socket.addEventListener("message", ({ data }) => {
    if (typeof data !== "string") return;
    let message;
    try { message = JSON.parse(data); } catch { return; }
    if (message.typ === "draft") return showRemoteDraft(message);
    const waiter = state.pending.shift();
    if (waiter) message.ok ? waiter.resolve(message.data) : waiter.reject(new Error(message.error?.message || "Request failed"));
  });
  socket.addEventListener("close", () => {
    if (state.socket === socket) state.socket = null;
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
  if (!state.room) el.floorStatus.textContent = "Open a room to see who holds Conch.";
  else if (!holder) el.floorStatus.textContent = "The floor is vacant. Raise your hand to speak.";
  else if (mine) el.floorStatus.textContent = "You hold Conch. Your next take can be wrapped.";
  else el.floorStatus.textContent = `${holder.agent} holds Conch on ${short(holder.node)}.`;
  el.takeButton.disabled = state.readOnly || !state.room || Boolean(holder);
  el.speech.disabled = !mine;
  el.wrapButton.disabled = !mine || !el.speech.value.trim();
  el.yieldButton.disabled = !mine;
  el.composeHint.textContent = state.readOnly
    ? "The committed history is available; live room mutations are reconnecting."
    : mine ? "Your draft is local until you wrap it." : "The committed grant controls this composer.";
  if (!mine) hideDraft();
}

async function takeFloor() {
  el.takeButton.disabled = true;
  try {
    await rpc({ typ: "raise_hand", room: state.room });
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
    const result = await response.json();
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
      const result = await response.json();
      if (!response.ok) throw new Error(result.error || `Join failed (${response.status}).`);
      await loadCatalog();
    } else if (ticket.token) {
      const response = await fetch(`/session/${ticket.id}`, {
        method: "POST",
        headers: { Authorization: `Bearer ${ticket.token}` },
        credentials: "same-origin",
        cache: "no-store",
      });
      if (!response.ok) throw new Error(`Session authorization failed (${response.status}).`);
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
    const response = await fetch(source, { redirect: "follow" });
    if (!response.ok) throw new Error(`Ticket fetch failed (${response.status}).`);
    return response.json();
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
  link.click();
  URL.revokeObjectURL(link.href);
  showToast("Invitation downloaded.");
}

async function copyMagnet() {
  if (!state.invitation) return;
  await navigator.clipboard.writeText(ticketMagnet(state.invitation));
  showToast("Private magnet copied.");
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
    openRoom(link.dataset.room);
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
    el.wrapButton.disabled = !el.speech.value.trim();
    el.draftText.textContent = el.speech.value;
    el.draftPreview.hidden = !el.speech.value;
  });
  el.transcript.addEventListener("scroll", () => {
    if (isNearBottom()) { state.newMessages = 0; renderNewMessages(); }
  }, { passive: true });
  el.newMessages.addEventListener("click", scrollToLatest);
  el.copyRoomId.addEventListener("click", async () => {
    if (!state.room) return;
    await navigator.clipboard.writeText(state.room);
    showToast("Room id copied.");
  });
  el.downloadTicket.addEventListener("click", downloadInvitation);
  el.copyMagnet.addEventListener("click", copyMagnet);
  el.finishTicket.addEventListener("click", closeInvitation);
  el.ticketDialog.addEventListener("cancel", () => { state.invitation = null; });
  el.roomsToggle.addEventListener("click", () => toggleRail("rooms"));
  el.peopleToggle.addEventListener("click", () => toggleRail("people"));
  window.addEventListener("popstate", () => {
    const room = roomFromPath();
    if (room) openRoom(room, false); else showHome(false);
  });
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
}

function closeMobileRails() {
  el.roomsRail.classList.remove("open");
  el.peopleRail.classList.remove("open");
  el.roomsToggle.setAttribute("aria-expanded", "false");
  el.peopleToggle.setAttribute("aria-expanded", "false");
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
  el.speech.disabled = busy;
  el.wrapButton.disabled = busy;
  el.yieldButton.disabled = busy;
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

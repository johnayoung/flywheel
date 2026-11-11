// flywheel hub web client: the connection -- link state, polling, and
// the hub-wide events WebSocket that pushes agent status; seen-tracking
// and the favicon / title badge.
"use strict";

// --- sockets ---------------------------------------------------------

// The header's link pill follows the focused pane.
// The focused pane's link state in the header (idle with none).
function syncLink() {
  const pane = focusedPane();
  if (pane && pane.sessionId) setLink(pane.link.kind, pane.link.text);
  else setLink("idle");
}

function setLink(kind, text) {
  const pill = $("#link");
  pill.dataset.link = kind;
  pill.textContent = text || kind;
  // A narrow header shows the pill as its dot: the word stays its tooltip.
  pill.title = `connection: ${text || kind}`;
}

// Hub-wide messages (an error not about one pane).
function showBanner(text, actions, calm) {
  $("#banner").hidden = false;
  $("#banner").classList.toggle("calm", !!calm);
  $("#banner-text").textContent = text;
  $("#banner-actions").replaceChildren(...(actions || []).filter(Boolean));
}

// Leave the workspace: every pane closes (the layout stays on the hub).
function detach(message, title) {
  for (const pane of livePanes()) pane.dispose();
  state.panes.clear();
  state.solo = null;
  state.focusId = null;
  state.view = null;
  state.stageKey = "";
  state.stageView = null;
  $("#terminal").replaceChildren();
  state.attachedMeta = null;
  history.replaceState(null, "", window.location.pathname);
  $("#banner").hidden = true;
  setLink("idle");
  state.notice = message || "";
  state.noticeTitle = (message && title) || "";
  renderTabs();
  render();
  sendView();
}

// What to say about the attached session should it disappear.
function rememberAttached() {
  const session = sessionById(state.attached);
  if (!session) return;
  const workspace = workspaceById(session.workspace_id);
  state.attachedMeta = {
    name: session.name,
    workspaceId: session.workspace_id,
    workspaceName: workspace ? workspace.name : "",
  };
}

const wakeUp = () => {
  for (const pane of livePanes()) pane.wake();
};
window.addEventListener("online", wakeUp);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") wakeUp();
});

// Lite mode (netlink.js): every pane's socket is paced as lite mode and
// its visibility say (a hidden pane gets no updates), and the header's
// lite pill (always on a phone; elsewhere while lite is on) shows it.
function syncLite() {
  for (const pane of livePanes()) pane.syncMode();
  const pill = $("#lite-toggle");
  if (!pill) return;
  const on = net.lite();
  pill.hidden = !(on || phone());
  pill.setAttribute("aria-pressed", String(on));
  pill.classList.toggle("on", on);
  // The glyph says what was measured (two bars: a slow link), the word
  // and the pressed state whether lite is on.
  const use = pill.querySelector("use");
  if (use) use.setAttribute("href", net.slowLink() ? "#i-signal-low" : "#i-signal");
  pill.setAttribute("aria-label", `Lite mode ${on ? "on" : "off"}${net.slowLink() ? " (slow link)" : ""}`);
  pill.title = `${net.describe()}. Tap to turn it ${on ? "off" : "on"} on this device (Settings > Terminal: auto).`;
}
net.onChange(syncLite);
document.addEventListener("visibilitychange", syncLite);
// Panes park and unpark (tabs, zoom) without an event of their own.
setInterval(syncLite, 1000);

// Crossing the phone breakpoint swaps the solo pane and the layout.
// `state.attached` already reads the new side of the breakpoint here, so
// the session on screen is taken from the side being left.
window.matchMedia(NARROW).addEventListener("change", () => {
  if (phone()) {
    const was = state.panes.get(state.focusId);
    const shown = was && was.sessionId;
    renderStage();
    if (shown && sessionById(shown)) showSolo(shown);
  } else {
    const shown = state.solo && state.solo.sessionId;
    if (state.solo) state.solo.dispose();
    state.solo = null;
    const layout = viewLayout();
    const hit = shown && layout && layout.tabs
      .map((t) => ({ tab: t, leaf: L.paneShowing(t.root, shown) }))
      .find((h) => h.leaf);
    if (hit) {
      const view = viewOf(state.view);
      view.tab = hit.tab.id;
      view.focus[hit.tab.id] = hit.leaf.id;
      saveViews();
    }
    state.stageKey = "";
    renderStage();
  }
  // The sidebar lists differently on each side (see renderWorkspace).
  render();
});

// --- events: pushed agent status -------------------------------------

// Someone is at this page: it is visible and focused, and was touched,
// typed into, or pointed at in the last IDLE_MS. A tab left focused on a
// desk monitor while its user is away must not count as watching (it
// would hold off every push about the session it shows).
// A turn on screen that someone was here for is watched to its end,
// though: whoever sat through an agent's work without touching anything
// is reading the reply when it finishes (seen, not "done"), up to
// WATCH_MS since their last input (past that, they have gone). One that
// started while nobody was here (a queued prompt) is not: the watch is
// per session, so in a split one pane's watched turn does not carry
// another's.
const IDLE_MS = 5 * 60 * 1000;
const WATCH_MS = 30 * 60 * 1000;
let lastInput = Date.now();
let idleTimers = [];
const watched = new Set();
function workingOnScreen() {
  const ids = [state.attached, ...livePanes().filter((p) => onStage(p)).map((p) => p.sessionId)];
  return new Set(ids.filter((id) => {
    const session = id && sessionById(id);
    const status = session && agentStatus(session);
    return !!status && status.status === "working";
  }));
}
// The sessions on screen still watched (none once WATCH_MS passed): a
// turn stays watched from someone's input during it to its end.
function watching() {
  const quiet = Date.now() - lastInput;
  const working = workingOnScreen();
  for (const id of watched) if (!working.has(id)) watched.delete(id);
  if (quiet < IDLE_MS) for (const id of working) watched.add(id);
  return quiet < WATCH_MS ? watched : new Set();
}
function idle() {
  const quiet = Date.now() - lastInput;
  return watching().size === 0 && quiet >= IDLE_MS;
}
function present() {
  return document.visibilityState === "visible" && document.hasFocus() && !idle();
}
function armIdle() {
  for (const timer of idleTimers) clearTimeout(timer);
  idleTimers = [IDLE_MS, WATCH_MS].map((ms) => setTimeout(() => sendView(), ms + 100));
}
function noteInput(event) {
  // Pointer moves are many: one a second is plenty.
  if (event.type === "pointermove" && Date.now() - lastInput < 1000) return;
  const wasIdle = idle();
  lastInput = Date.now();
  armIdle();
  if (wasIdle) sendView();
}
for (const type of ["keydown", "pointerdown", "pointermove", "wheel", "touchstart"]) {
  window.addEventListener(type, noteInput, { capture: true, passive: true });
}
armIdle();
// Coming back to the page counts too: its tab shown, its window focused,
// a session link opened (each is someone's doing). A look at a finished
// turn after a long break marks it seen without a click.
function arrive() {
  lastInput = Date.now();
  armIdle();
}

// What this page shows, for seen-tracking: the attached session while
// someone is at the page (above), else nothing (a window left visible
// behind an editor is not being looked at, so a turn that finishes
// there stays "done").
// Every pane on screen counts (a split shows several sessions at once;
// panes behind a zoom or in other tabs do not).
// On stage = inside #terminal. Panes behind a zoom or in another tab are
// parked in the hidden #pane-park, which is still connected to the
// document, so isConnected cannot tell them apart.
function onStage(pane) {
  return $("#terminal").contains(pane.el);
}

function currentView() {
  if (!present()) return { session_id: null, visible: [] };
  // Present only through a watch (no input for IDLE_MS): just the
  // watched sessions count as looked at.
  const only = Date.now() - lastInput >= IDLE_MS ? watching() : null;
  const counts = (id) => !only || only.has(id);
  const session = sessionById(state.attached);
  const focused = session && session.live && counts(session.id) ? session.id : null;
  const visible = livePanes()
    .filter((p) => p.sessionId && p.sessionId !== focused && onStage(p) && sessionById(p.sessionId)?.live && counts(p.sessionId))
    .map((p) => p.sessionId);
  return { session_id: focused, visible };
}

function sendView(force) {
  machines.sendView(force);
  const socket = state.events;
  if (!socket || socket.readyState !== WebSocket.OPEN) return;
  const all = currentView();
  const here = (id) => id && !machines.machineOf(id);
  // `present`: someone is at the page (the hub's "while you were away").
  const view = { session_id: here(all.session_id) ? all.session_id : null, visible: all.visible.filter(here), present: present() };
  const key = JSON.stringify(view);
  if (!force && key === state.viewing) return;
  state.viewing = key;
  socket.send(JSON.stringify({ type: "view", ...view }));
}

// A status push repaints the sidebar -- except under an inline rename,
// which must not be torn down mid-typing (it repaints when it ends).
// Pushes come in bursts (a turn ending: Stop, then the badge, then the
// usage; many agents at once): they repaint once per STATUS_COALESCE_MS.
// A timer rather than a frame, so a background tab's title and favicon
// counts still follow (frames do not run there).
const STATUS_COALESCE_MS = 50;
let statusTimer = 0;
function renderStatus() {
  if (statusTimer) return;
  statusTimer = setTimeout(() => {
    statusTimer = 0;
    renderStatusNow();
  }, STATUS_COALESCE_MS);
}

function renderStatusNow() {
  if (state.renaming) {
    attention.render();
    renderMain();
  } else {
    render();
  }
}

function startPolling() {
  if (!state.poller) state.poller = setInterval(() => refresh(false).catch(() => {}), 4000);
}

function stopPolling() {
  clearInterval(state.poller);
  state.poller = null;
}

// One socket for the whole hub. While it is up, list changes and status
// arrive as pushes; while it is down, the sidebar falls back to polling.
function connectEvents() {
  clearTimeout(state.eventsTimer);
  const scheme = window.location.protocol === "https:" ? "wss" : "ws";
  const socket = new WebSocket(
    `${scheme}://${window.location.host}/api/events?client=${encodeURIComponent(CLIENT_ID)}`,
  );
  state.events = socket;
  socket.onopen = () => {
    state.eventsRetry = 0;
    stopPolling();
    sendView(true);
    // Anything that changed while the socket was down.
    refresh(false).catch(() => {});
    net.defer("flywheel", () => flywheel.reload());
    fanout.reload();
  };
  socket.onmessage = (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch (_) {
      return;
    }
    if (message.type === "snapshot") {
      loadTelemetry();
      // This hub's statuses; the machines' stay (they have their own
      // sockets).
      const local = (id) => !machines.machineOf(id);
      for (const id of Object.keys(state.agent)) if (local(id)) delete state.agent[id];
      for (const id of Object.keys(state.rollup)) if (local(id)) delete state.rollup[id];
      Object.assign(state.agent, message.sessions || {});
      Object.assign(state.rollup, message.workspaces || {});
      if (state.restarted) {
        // Back after a live handoff (`fw hub upgrade`, close 1012).
        state.restarted = false;
        const was = state.hubVersion && state.hubVersion !== message.version ? ` (was ${state.hubVersion})` : "";
        commands.toast(`Hub upgraded to flywheel ${message.version}${was}; every session kept running.`, { timeout: 5000 });
      }
      if (message.version) state.hubVersion = message.version;
      renderStatus();
    } else if (message.type === "status") {
      applyStatus(message);
    } else if (message.type === "sessions") {
      net.relist(() => refresh(false).catch(() => {}));
      // Machines added or removed here.
      machines.sync().then(() => renderSoon());
    } else {
      onPush(message, null);
    }
  };
  socket.onclose = (event) => {
    if (state.events !== socket) return;
    state.events = null;
    state.viewing = undefined;
    // 4001: the hub's token was rotated; 4002: this browser's login
    // ended (signed out elsewhere, expired). Either way: sign in again.
    if (event.code === 4001 || event.code === 4002) {
      const why = event.code === 4001 ? "rotated" : "ended";
      window.location.replace("/?signed_out=" + why + window.location.hash);
      return;
    }
    if (event.code === 1012) {
      // The hub process is being replaced: back in a moment.
      state.restarted = true;
      state.eventsRetry = 0;
      state.eventsTimer = setTimeout(connectEvents, 300);
      return;
    }
    startPolling();
    state.eventsRetry += 1;
    const delay = Math.min(10000, 500 * 2 ** Math.min(state.eventsRetry - 1, 5));
    state.eventsTimer = setTimeout(connectEvents, delay);
  };
}

// A status push (this hub's or a machine's).
function applyStatus(message) {
  notify.onStatus(message.session_id, state.agent[message.session_id], message.status);
  if (message.status) state.agent[message.session_id] = message.status;
  else delete state.agent[message.session_id];
  if (message.workspace_id) {
    if (message.workspace_status) state.rollup[message.workspace_id] = message.workspace_status;
    else delete state.rollup[message.workspace_id];
  }
  renderStatus();
  handoff.onStatus();
  // A watched turn that ended may end the watch (see present()).
  sendView();
}

// The other pushes; `machine`: the machine that sent it (null: here).
function onPush(message, machine) {
  if (message.type === "commands") {
    commands.onEvent(message);
  } else if (message.type === "replies") {
    replies.onEvent(message);
  } else if (message.type === "usage") {
    launcher.onUsage(message);
  } else if (message.type === "flywheel" || message.type === "flywheel_reset") {
    flywheel.onEvent(message, machine);
    flywheelBoard.onFlywheel(message, machine);
  } else if (message.type === "layout") {
    onLayoutEvent(message);
  } else if (message.type === "fanout" && !machine) {
    fanout.onEvent(message);
  } else if (message.type === "activity") {
    activity.onRow(message.row, machine);
    pullRequests.onActivity(message.row);
  } else if (message.type === "telemetry") {
    noteTelemetry(message.session, machine);
    fleet.onSnapshot(message.session, machine);
    work.onSnapshot(message.session, machine);
  } else if (message.type === "away" && !machine) {
    activity.onAway(message);
  } else if (message.type === "digest" && !machine) {
    digests.onPush(message);
  } else if (!machine && ["settings", "config", "plugins", "plugin_toast"].includes(message.type)) {
    settings.onPush(message);
    if (message.type === "config") digests.onConfig();
  }
}

const eventsWake = () => {
  if (!state.events) connectEvents();
  machines.wake();
};
window.addEventListener("online", eventsWake);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") {
    eventsWake();
    arrive();
  }
  sendView();
});
window.addEventListener("focus", () => {
  arrive();
  sendView();
});
window.addEventListener("hashchange", () => {
  arrive();
  sendView();
});
window.addEventListener("blur", () => sendView());

// Tab title and favicon carry what needs you: blocked and done counts.
const favicon = {
  link: document.querySelector('link[rel="icon"]'),
  image: null,
  drawn: "",
};

// Blocked and done agents (optionally all but one session).
// What needs you elsewhere, counted as Needs you lists it: an open
// fan-out's finished candidates are one item, its row (until someone opens
// its board), not one per candidate.
function attentionCounts(except) {
  let blocked = 0;
  let done = 0;
  const graded = fanout.candidateSessions();
  for (const session of state.sessions) {
    if (session.id === except) continue;
    const agent = agentStatus(session);
    if (agent && agent.status === "blocked") blocked += 1;
    if (agent && agent.status === "done" && !graded.has(session.id)) done += 1;
  }
  done += fanout.attention().length;
  return { blocked, done };
}

function titlePrefix({ blocked, done }) {
  const parts = [];
  if (blocked) parts.push(`${blocked} blocked`);
  if (done) parts.push(`${done} done`);
  return parts.length ? `(${parts.join(", ")}) ` : "";
}

function drawFavicon({ blocked, done }) {
  const key = `${blocked}/${done}`;
  if (key === favicon.drawn || !favicon.link) return;
  if (!blocked && !done) {
    favicon.drawn = key;
    favicon.link.type = "image/svg+xml";
    favicon.link.href = "/assets/icon.svg";
    return;
  }
  if (!favicon.image) {
    favicon.image = new Image();
    favicon.image.onload = () => {
      favicon.drawn = "";
      drawFavicon(attentionCounts());
    };
    favicon.image.src = "/assets/icon.svg";
    return;
  }
  if (!favicon.image.complete) return;
  const size = 64;
  const canvas = document.createElement("canvas");
  canvas.width = size;
  canvas.height = size;
  const ctx = canvas.getContext("2d");
  ctx.drawImage(favicon.image, 0, 0, size, size);
  // One badge per kind: blocked (a person is needed) top right, done
  // bottom right; a lone badge is drawn large so it reads at 16px.
  const badges = [];
  if (blocked) badges.push({ count: blocked, color: "#ff3d5e" });
  if (done) badges.push({ count: done, color: "#d9a93a" });
  const r = badges.length === 1 ? 22 : 16;
  badges.forEach(({ count, color }, i) => {
    const cx = size - r;
    const cy = badges.length === 1 ? r : i === 0 ? r : size - r;
    ctx.beginPath();
    ctx.arc(cx, cy, r + 3, 0, Math.PI * 2);
    ctx.fillStyle = "#0c0d0c";
    ctx.fill();
    ctx.beginPath();
    ctx.arc(cx, cy, r, 0, Math.PI * 2);
    ctx.fillStyle = color;
    ctx.fill();
    ctx.fillStyle = "#0c0d0c";
    ctx.font = `bold ${Math.round(r * 1.5)}px ui-monospace, Menlo, Consolas, monospace`;
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.fillText(count > 9 ? "9+" : String(count), cx, cy + 1);
  });
  favicon.drawn = key;
  favicon.link.type = "image/png";
  favicon.link.href = canvas.toDataURL("image/png");
}

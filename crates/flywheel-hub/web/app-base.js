// flywheel hub web client: helpers, client state, the API, the feature
// modules' wiring (commands.js, flywheel.js, review.js, ...), and the
// theme. First of the app-*.js files (see app.js for how they share
// one scope).
"use strict";

// --- helpers ---------------------------------------------------------

const $ = (sel, root = document) => root.querySelector(sel);
const SVG_NS = "http://www.w3.org/2000/svg";

function el(tag, props, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === undefined || value === null) continue;
    if (key === "dataset") Object.assign(node.dataset, value);
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else if (key in node && key !== "role") node[key] = value;
    else if (value !== false) node.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children.flat()) {
    if (child !== null && child !== undefined && child !== false) node.append(child);
  }
  return node;
}

function icon(id) {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS(SVG_NS, "use");
  use.setAttribute("href", `#i-${id}`);
  svg.append(use);
  return svg;
}

// An agent CLI session (not a shell).
const isAgent = (s) => !!s && (s.kind === "claude" || s.kind === "codex");

// A duration as the UI writes it everywhere: 42s, 5m, 3h, 2d.
function span(seconds) {
  if (seconds < 60) return `${Math.floor(seconds)}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

// How long ago `iso` was ("" when it is not a time).
function age(iso) {
  const seconds = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  return Number.isFinite(seconds) ? span(seconds) : "";
}

function iconButton(id, title, onclick, danger) {
  return el(
    "button",
    {
      type: "button",
      className: `icon${danger ? " danger" : ""}`,
      title,
      "aria-label": title,
      onclick: (event) => {
        event.stopPropagation();
        onclick(event);
      },
    },
    icon(id),
  );
}

// macOS types characters with Option ([ ] | { } on many layouts, dead
// keys on US), so plain Option chords belong to the terminal there; the
// hub's chords use Ctrl+Option instead.
const IS_MAC = /mac|iphone|ipad/i.test(
  (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "",
);
const MOD = IS_MAC ? "Ctrl Opt" : "Alt";

const basename = (path) => path.replace(/\/+$/, "").split("/").pop() || path;
// A workspace folder's display name: its label, else its directory name.
const folderLabel = (folder) => folder.name || basename(folder.path);
const pad2 = (n) => String(n).padStart(2, "0");
const KIND_GLYPH = { shell: "shell", claude: "claude", codex: "codex" };
const glyphFor = (kind) => KIND_GLYPH[kind] || "shell";

// Below this the sidebar is a drawer (keep in step with app.css).
const NARROW = "(max-width: 760px)";

const store = {
  get(key, fallback) {
    try {
      const raw = window.localStorage.getItem(`fw-hub.${key}`);
      return raw === null ? fallback : JSON.parse(raw);
    } catch (_) {
      return fallback;
    }
  },
  set(key, value) {
    try {
      window.localStorage.setItem(`fw-hub.${key}`, JSON.stringify(value));
    } catch (_) {
      // Private mode / quota: preferences just do not persist.
    }
  },
};

// Settings (settings.js): preferences the hub keeps for you, this
// device's overrides, the keymap every shortcut dispatches on, plugins,
// and the hub.toml view. Made first: the theme and keys below read it.
const settings = window.FwHubSettings({
  $, el, icon, store, IS_MAC,
  rawApi: (...args) => rawApi(...args),
  // Settings > Digest (digest.js, created further down).
  digestSection: (rerender) => digests.settingsSection(rerender),
  toast: (...args) => commands.toast(...args),
  // The attached session when it is this hub's (plugins run here).
  attachedLocal: () => {
    const session = sessionById(state.attached);
    return session && !session.machine ? session.id : null;
  },
});
// The link to the hub and lite mode (netlink.js): weak links get paced
// terminals, paused hidden panes, and deferred fetches.
const net = window.FwHubNet({ prefs: settings });
// A shortcut hint from the keymap (never a hardcoded key: a rebind or
// unbind in Settings shows everywhere). `keyHint(id)`: the key, "" when
// unbound; `withKey(text, id)`: "text (key)", or just text.
const keyHint = (id) => settings.keyOf(id);
const withKey = (text, id) => window.FwHubFmt.withKey(text, keyHint(id));


// --- state -----------------------------------------------------------

const state = {
  workspaces: [],
  sessions: [],
  kinds: null, // last /api/kinds answer
  // attached / exited / replaying: the focused pane's (getters, below).
  view: null, // id of the workspace whose layout the stage shows
  layouts: {}, // workspace id -> layout shown (the hub's plus edits being saved)
  confirmed: {}, // workspace id -> layout as the hub last said
  layoutQueue: {}, // workspace id -> edits waiting to be saved, in order
  layoutLoading: new Set(), // workspace ids whose layout is being fetched
  panes: new Map(), // pane id -> pane (panes.js) of the active tab (desktop)
  focusId: null, // the focused pane's id (desktop)
  solo: null, // the phone's one pane
  stageKey: "", // shape of the tree on the stage
  stageView: null, // the workspace the desktop stage was last built for
  currentWorkspace: store.get("workspace", null), // last workspace touched
  collapsed: new Set(store.get("collapsed", [])),
  repoChoice: store.get("repo-choice", {}), // workspace id -> repo
  wsErrors: {}, // workspace id -> last action error
  renaming: null, // session id being renamed inline
  snapshot: "",
  offline: 0, // consecutive failed refreshes
  notice: "", // why the last session detached, shown on the empty stage
  noticeTitle: "", // headline for that notice (e.g. "Session removed")
  dividerHeld: new Set(), // dividers being dragged or key-resized (terminal fits wait)
  attachedMeta: null, // {name, workspaceId, workspaceName} of the attached session, kept to explain its removal
  closing: new Set(), // session ids this browser is removing (no "removed elsewhere" notice)
  refreshSeq: 0, // refreshes started (each response is tagged with its number)
  refreshApplied: 0, // the newest refresh whose response was applied
  painted: false, // first sidebar paint done (entry animation plays once)
  agent: {}, // session id -> {status, detail, since}, as this client sees it
  rollup: {}, // workspace id -> most urgent agent status
  events: null, // the hub-wide events socket
  eventsRetry: 0,
  // The events socket closed for a hub restart (1012); the version it served.
  restarted: false,
  hubVersion: null,
  refreshRetry: null, // a failed refresh's retry while the events socket is up
  eventsTimer: null,
  poller: null, // REST polling, only while the events socket is down
  viewing: undefined, // last view sent on the events socket
};

// This browser's identity for seen-tracking: a finished agent turn is
// "done" until this browser has looked at it, then "idle".
const CLIENT_ID = (() => {
  const kept = store.get("client", null);
  if (typeof kept === "string" && /^[A-Za-z0-9_-]{8,64}$/.test(kept)) return kept;
  const bytes = new Uint8Array(16);
  window.crypto.getRandomValues(bytes);
  const id = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  store.set("client", id);
  return id;
})();

const sessionById = (id) => state.sessions.find((s) => s.id === id);
const workspaceById = (id) => state.workspaces.find((w) => w.id === id);

// Sessions in sidebar order (workspace order, then creation order).
function orderedSessions() {
  const out = [];
  for (const workspace of state.workspaces) {
    for (const session of state.sessions) {
      if (session.workspace_id === workspace.id) out.push(session);
    }
  }
  return out;
}

// Sessions with a terminal on the hub (running, or ended with their
// scrollback), in sidebar order: what Alt+1..9 and Alt+Up/Down reach.
const attachable = () => orderedSessions().filter((s) => s.live);

// A running agent's status, as pushed for this client.
const AGENT_LOOK = {
  working: { dot: "working", label: "working", tone: "live" },
  blocked: { dot: "blocked", label: "blocked", tone: "bad" },
  done: { dot: "agent-done", label: "done", tone: "brass" },
  idle: { dot: "idle", label: "idle", tone: "" },
};

// A session this hub can reach now: its own, or one on a machine that
// is not offline or refusing this hub (an unreachable machine's lists
// are its last known ones).
const reachable = (session) => !session.machine || machines.reachable(session.machine);
// Running now, on a machine this page reaches (the header's "NN live"):
// an exited session keeps its `live` record.
const runningNow = (session) => !!session.live && session.state === "running" && reachable(session);

function agentStatus(session) {
  if (session.kind === "shell" || session.state !== "running" || !session.live) return null;
  if (!reachable(session)) return null;
  const pushed = state.agent[session.id];
  return pushed && AGENT_LOOK[pushed.status] ? pushed : null;
}

// The record's own state, kept across hub restarts (a session without a
// terminal on this hub still shows how it ended); a running agent shows
// what it is doing instead.
function statusLook(session) {
  if (!reachable(session)) {
    const link = machines.linkOf(session.machine) || "offline";
    return {
      dot: "gone",
      label: link,
      tone: "",
      title: `machine ${session.machine} is ${link}: this is its last known state`,
    };
  }
  const agent = agentStatus(session);
  if (agent) {
    const look = AGENT_LOOK[agent.status];
    return {
      ...look,
      detail: agent.detail || "",
      title: agent.detail ? `${look.label}: ${agent.detail}` : look.label,
    };
  }
  // A worker session whose worker exited is its shell now: say how the
  // worker ended (a scoped run's 0 = every named task verified, 2 = refused).
  const worker = session.worker;
  if (session.state === "running" && session.live && worker && !worker.running) {
    // An autopilot session (its wrapper's marker) ended its daemon.
    if (Array.isArray(session.command) && session.command[3] === "fw-autopilot") {
      const code = worker.exit_status;
      const title = code == null ? "fw autopilot exited" : `fw autopilot exited with status ${code}; its shell is still open`;
      if (code == null || code === 0) return { dot: "done", label: "autopilot stopped", tone: "", title };
      return { dot: "bad", label: `autopilot exit ${code}`, tone: "bad", title };
    }
    const scoped = Array.isArray(session.command) && session.command.some((a) => String(a).startsWith("--task="));
    const code = worker.exit_status;
    const title = code == null ? "fw worker exited" : `fw worker exited with status ${code}; its shell is still open`;
    if (code === 0) return { dot: "done", label: scoped ? "verified" : "worker done", tone: "", title };
    if (code == null) return { dot: "done", label: "worker exited", tone: "", title };
    const label = scoped && code === 1 ? "not verified" : scoped && code === 2 ? "refused" : `worker exit ${code}`;
    return { dot: "bad", label, tone: "bad", title };
  }
  if (session.state === "running") {
    return session.live
      ? { dot: "live", label: "live", tone: "live" }
      : { dot: "gone", label: "gone", tone: "" };
  }
  if (session.state === "exited") {
    if (session.signal) return { dot: "bad", label: session.signal, tone: "bad" };
    // Code unknown: it exited after a live handoff (not our child).
    if (session.code == null) return { dot: "done", label: "exited", tone: "" };
    return session.code === 0
      ? { dot: "done", label: "done", tone: "" }
      : { dot: "bad", label: `exit ${session.code}`, tone: "bad" };
  }
  if (session.restore_error) return { dot: "bad", label: "not restored", tone: "bad" };
  return { dot: "stopped", label: "stopped", tone: "" };
}

const exitHow = (info) =>
  info.signal ? `killed by ${info.signal}` : info.code == null ? "exit code unknown" : `exit code ${info.code}`;

// Why a session has no terminal on this hub.
function goneText(session) {
  if (session.state === "stopped" && session.restore_error) {
    return `${session.name} was running when the hub stopped, but could not be restored: ${session.restore_error}.`;
  }
  if (session.state === "stopped") {
    return `${session.name} was running when the hub stopped; its terminal did not survive the restart.`;
  }
  if (session.state === "exited") {
    return `${session.name} ended (${exitHow(session)}) before the hub restarted; its terminal is gone.`;
  }
  return `${session.name} has no terminal on this hub.`;
}

// --- API -------------------------------------------------------------

// `path` as given (a machine's calls already under /m/<machine>).
// `signal` aborts it (a search fanned out to machines gives up on one).
async function rawApi(method, path, body, signal) {
  const init = { method, credentials: "same-origin", headers: {}, signal };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  const response = await fetch(path, init);
  // Always this hub's own refusal: a machine's is a 502 (the proxy).
  if (response.status === 401) {
    window.location.replace("/" + window.location.hash);
    throw new Error("signed out");
  }
  const text = await response.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch (_) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  if (!response.ok) {
    const error = new Error((data && data.error) || response.statusText);
    error.status = response.status;
    if (data && data.machine_state) error.machineState = data.machine_state;
    throw error;
  }
  return data;
}

// Every call naming a remote workspace or session goes to its machine
// (machines.js); the rest to this hub.
const api = (method, path, body) => rawApi(method, machines.route(method, path, body), body);

// Other machines' hubs (machines.js): their workspaces and sessions
// beside this hub's, through its proxy.
const machines = window.FwHubMachines({
  $, el, icon, state, rawApi,
  clientId: CLIENT_ID,
  toast: (...args) => commands.toast(...args),
  confirmDialog: (options) => confirmDialog(options),
  currentView: () => currentView(),
  // Lists or connection states changed: merge and repaint.
  onChange: () => renderSoon(),
  onStatusChange: () => renderStatus(),
  onStatus: (message) => applyStatus(message),
  onEvent: (message, machine) => onPush(message, machine),
  onMachineFlywheel: (machine) => {
    flywheel.reloadMachine(machine);
    activity.onMachine(machine);
  },
  signedOut: (why) => window.location.replace("/?signed_out=" + why + window.location.hash),
});

// The Commands drawer (commands.js): agents' suggested code blocks.
const commands = window.FwHubCommands({
  $,
  el,
  icon,
  isAgent,
  toastsOn: () => settings.get("notifications.toasts", true) !== false,
  api,
  state,
  sessionById,
  workspaceById,
  keyOf: (id) => keyHint(id),
  openSession: (id) => openSession(id),
  flipTo: (id, from) => flipTo(id, from),
  refresh: () => refresh(),
  termSize: () => sizeHint(),
  focusTerminal: () => focusTerminal(),
  onOpen: () => closeOverlays("commands"),
  // "To agent" on a block (handoff.js).
  handoff: () => handoff,
});

// One overlay over the terminal at a time: opening a drawer closes the
// others and the "why?" popover (which would cover the drawer's top).
function closeOverlays(keep) {
  if (keep !== "commands" && commands.isOpen()) commands.toggle();
  if (keep !== "flywheel") flywheel.close();
  if (keep !== "review") review.close();
  if (keep !== "queue") promptQueue.close();
  explain.close();
}

// The Flywheel panel (flywheel.js): verified-loop state for the
// flywheel-adopted folders.
const flywheel = window.FwHubFlywheel({
  $, el, icon, api, age, span, state, workspaceById, sessionById, folderLabel,
  keyOf: (id) => settings.keyOf(id),
  machinePrefix: (machine) => machines.prefix(machine),
  openSession: (id, place) => openSession(id, place),
  toast: commands.toast,
  confirmDialog: (options) => confirmDialog(options),
  refresh: () => refresh(),
  rerender: () => render(),
  onOpen: () => closeOverlays("flywheel"),
  focusTerminal: () => focusTerminal(),
  // "Fan out this task" (fanout-task.js, below).
  fanOutTask: (folder, workspaceId, taskId) => fanoutTask.fanOutTask(folder, workspaceId, taskId),
  termSize: () => sizeHint(),
  // Autopilot work newly waiting on a gate (notify.js, below).
  notifyGate: (folder, label, count) => notify.gate(folder, label, count),
});

// Verified fan-outs (fanout.js): one prompt to several agents in
// worktrees, graded by the hub, ranked, the winner merged.
const fanout = window.FwHubFanout({
  $, el, icon, api, state, age, sessionById, workspaceById, folderLabel,
  openSession: (id) => openSession(id),
  openReview: async (id) => {
    await openSession(id);
    review.open(id);
  },
  toast: (...args) => commands.toast(...args),
  refresh: () => refresh().catch(() => {}),
  termSize: () => sizeHint(),
  focusTerminal: () => focusTerminal(),
  // The Needs-you list, menu badge and title all count fan-outs.
  onChange: () => render(),
  // Promote / task-seeded launch (fanout-task.js).
  task: () => fanoutTask,
});
// A fan-out and a flywheel task, both ways (fanout-task.js): Promote on
// the board, "fan out" on the Flywheel panel's tasks.
const fanoutTask = window.FwHubFanoutTask({
  el, api,
  toast: (...args) => commands.toast(...args),
  focusTerminal: () => focusTerminal(),
  openLaunch: (workspaceId, seed) => fanout.openLaunch(workspaceId, seed),
  // The board steps aside for the loop's panel and the worker's terminal.
  openFlywheel: (folder, workspaceId) => {
    fanout.close();
    flywheel.open(folder, workspaceId);
  },
  startWorker: (folder, workspaceId, taskId) => {
    fanout.close();
    return flywheel.startWorkerIn(folder, workspaceId, taskId);
  },
  pendingTasks: (folder, except) => flywheel.pendingTasks(folder, except),
  onPromoted: () => fanout.reload(),
});

// The phone and alerts modules (pwa.js, attention.js, notify.js,
// mobile.js).
const pwa = window.FwHubPwa();
const attention = window.FwHubAttention({
  $, el, icon, age, state, workspaceById, agentStatus, openSession: (id) => openSession(id),
  // A phone lists these rows here instead of under their workspace: they
  // carry their number (Alt+N) there, so the workspace's list (an
  // orphaned companion's included) does not start past a gap.
  numberOf: (id) => {
    if (!phone()) return null;
    const i = attachable().findIndex((s) => s.id === id);
    return i >= 0 && i < 9 ? i + 1 : null;
  },
  flywheel,
  fanout,
  // Stalled prompt queues (queue.js).
  queue: () => promptQueue,
});
const notify = window.FwHubNotify({
  $, state, store, prefs: settings, sessionById, workspaceById, pwa, api, attention, toast: commands.toast,
  openSession: (id) => openSession(id),
  openFolder: (folder) => flywheel.open(folder),
  present: () => present(),
});
// Sibling worktrees (worktree.js): the new-session fields, row badges,
// and the keep / remove close dialog.
const worktrees = window.FwHubWorktree({ $, el, icon, api });
// The agent launcher and cost meter (launcher.js): the form's agent
// fields, launch templates, quick launch, and token usage.
const launcher = window.FwHubLauncher({
  $, el, api, state, worktrees,
  // Usage rollups wait while lite (netlink.js).
  defer: (key, fn) => net.defer(key, fn),
  // The new-session form's pre-filled agent options (Settings: agents).
  agentDefaults: (kind) => ({
    model: settings.get(`agents.${kind}.model`, "") || "",
    permission_mode: kind === "claude" ? settings.get("agents.claude.permission_mode", "") || "" : "",
    approval: kind === "codex" ? settings.get("agents.codex.approval", "") || "" : "",
    sandbox: kind === "codex" ? settings.get("agents.codex.sandbox", "") || "" : "",
  }),
  // What hub.toml's [agents.*] gives a launch naming none.
  hubDefault: (id) => settings.hubDefault(id),
  remoteUsage: () => machines.usage(),
  // The spend strip counts this hub only: named so once machines show.
  localLabel: () => (machines.any() ? machines.host() || "this hub" : ""),
  toast: (...args) => commands.toast(...args),
  sessionById: (id) => sessionById(id),
  workspaceById: (id) => workspaceById(id),
  selectedKind: () => selectedKind(),
  selectKind: (kind) => {
    const radio = sessionForm.querySelector(`input[name="kind"][value="${kind}"]`);
    if (radio && !radio.disabled) radio.checked = true;
    updateSessionPlaceholder();
  },
  refresh: () => refresh(),
  createSession: (body, place) => createSession(body, place),
  // Never under a half-typed sidebar field (a rename): the next
  // render shows it.
  onRollups: () => {
    const busy = $("#sidebar").contains(document.activeElement) &&
      document.activeElement.matches("input, select, textarea");
    if (!busy) render();
  },
  // The push carries the usage (set on the session already): redraw
  // from it, never refetch every list per push.
  onSessionUsage: (id) => {
    if (state.attached === id) launcher.renderHeader(sessionById(id));
    clearTimeout(state.usageRender);
    state.usageRender = setTimeout(() => {
      const busy = $("#sidebar").contains(document.activeElement) &&
        document.activeElement.matches("input, select, textarea");
      if (!busy) render();
    }, 300);
  },
  confirmDialog: (options) => confirmDialog(options),
});
// Pull requests and CI (pr.js): the row badge and the Review drawer's PR
// section (checks, logs, ask the agent to fix CI, re-run, ready, merge).
const pullRequests = window.FwHubPr({
  el, icon, api, age, sessionById,
  toast: (...args) => commands.toast(...args),
  confirmDialog: (options) => confirmDialog(options),
  openSession: (id) => openSession(id),
  openReview: (id) => review.open(id),
  attached: () => state.attached,
  notify: () => notify,
});
// Review (review.js): a session's commits, diffs, and the guarded
// commit / push / PR / rebase / merge actions.
const review = window.FwHubReview({
  $, el, icon, api, state, sessionById, age,
  keyOf: (id) => keyHint(id),
  toast: commands.toast,
  copyText: (text) => commands.copyText(text),
  confirmDialog: (options) => confirmDialog(options),
  closeSession: (id) => closeSession(id),
  refresh: () => refresh(),
  focusTerminal: () => focusTerminal(),
  onOpen: () => closeOverlays("review"),
  // A fan-out candidate's review lands it through the board.
  openFanout: (id) => fanout.open(id),
  // The peer-review panel and "Send diff to agent" (handoff.js).
  handoff: () => handoff,
  // The PR section (pr.js).
  pr: () => pullRequests,
});
// The "why?" beside the status pill (explain.js).
const explain = window.FwHubExplain({ $, el, api, allowAlways: (id) => perms.allowFor(id) });
// Search across sessions (search.js): scrollback, transcripts, commands.
const search = window.FwHubSearch({
  $, el, icon, api, rawApi, machines, state, sessionById, workspaceById, age, glyphFor,
  keyOf: (id) => settings.keyOf(id),
  toast: commands.toast,
  openSession: (id) => openSession(id),
  openCommandsAt: (id, seq, all) => {
    review.close();
    flywheel.close();
    commands.openAt(id, seq, all);
  },
  livePanes: () => livePanes(),
  focusTerminal: () => focusTerminal(),
  onOpen: () => explain.close(),
  // "Send to agent" on a transcript message (handoff.js).
  handoff: () => handoff,
});

// Cross-agent handoff and review pairing (handoff.js): "Send to agent"
// from a selection, a transcript message, a suggested block, or a diff,
// and the read-only reviewer of the other kind.
const handoff = window.FwHubHandoff({
  $, el, icon, api, age, state, sessionById, workspaceById, isAgent, agentStatus,
  toast: (...args) => commands.toast(...args),
  copyText: (text) => commands.copyText(text),
  keyOf: (id) => settings.keyOf(id),
  openSession: (id) => openSession(id),
  refresh: () => refresh().catch(() => {}),
  // The focused terminal's selection ("" when none).
  selection: () => {
    const term = focusedTerm();
    return term && term.hasSelection() ? term.getSelection() : "";
  },
});

// Prompt queues (queue.js): prompts typed into an agent session one
// finished turn at a time.
const promptQueue = window.FwHubQueue({
  $, el, icon, api, age, state, sessionById, isAgent,
  toast: (...args) => commands.toast(...args),
  keyOf: (id) => settings.keyOf(id),
  confirmDialog: (options) => confirmDialog(options),
  openSession: (id) => openSession(id),
  refresh: () => refresh().catch(() => {}),
  focusTerminal: () => focusTerminal(),
  onOpen: () => closeOverlays("queue"),
});

// The activity timeline and "while you were away" (activity.js).
const activity = window.FwHubActivity({
  $, el, icon, rawApi, state, sessionById, workspaceById, store,
  toast: (...args) => commands.toast(...args),
  openSession: (id) => openSession(id),
  openCommandsAt: (id, seq, all) => {
    review.close();
    flywheel.close();
    commands.openAt(id, seq, all);
  },
  openReview: (id) => review.open(id),
  openQueue: (id) => promptQueue.open(id),
  openFanout: (id) => fanout.open(id),
  openTranscript: (group, index) => search.openTranscript(group, index),
  onOpen: () => {
    if (search.isOpen()) search.close(false);
    explain.close();
  },
});

// Scheduled digests (digest.js): the digest card, its notification, and
// Settings > Digest.
const digests = window.FwHubDigest({
  $, el, icon, rawApi, store, pwa, sessionById, prefs: settings,
  toast: (...args) => commands.toast(...args),
  openSession: (id) => openSession(id),
  openGate: (run) => flywheel.reload().catch(() => {}).then(() => flywheel.openGate(run)).then((found) => {
    if (!found) commands.toast("That gate is no longer waiting: it was decided, or its run moved on.", { timeout: 6000 });
  }),
  openActivity: (since) => activity.open({ since }),
});

// Agent permission rules (perms.js): the Permissions sheet and "Always
// allow" for a blocked session's request.
const perms = window.FwHubPerms({
  el, rawApi, state, sessionById, workspaceById,
  toast: (...args) => commands.toast(...args),
});

// Read-only share links and recordings (sharing.js).
const sharing = window.FwHubSharing({
  $, el, rawApi, sessionById, span,
  toast: (...args) => commands.toast(...args),
  copyText: (text) => commands.copyText(text),
  confirmDialog: (options) => confirmDialog(options),
  refresh: () => refresh().catch(() => {}),
});

// `force` re-renders even when nothing changed. The periodic refresh
// passes false and also holds off while a sidebar field has focus, so a
// half-typed rename is never torn down under the user.
// Refreshes overlap (events, the poller, actions); each response is
// tagged so one answered after a newer one is dropped, and only a list
// fetched after the attach may declare the attached session gone.
async function refresh(force = true) {
  const seq = ++state.refreshSeq;
  let ws;
  let ss;
  try {
    [ws, ss] = await Promise.all([
      api("GET", "/api/workspaces"),
      api("GET", "/api/sessions"),
      force ? machines.reloadAll() : null,
    ]);
  } catch (error) {
    if (seq < state.refreshApplied) return;
    state.offline += 1;
    $("#offline").hidden = state.offline < 2;
    // No poller runs while the events socket is up, and no push may come
    // to retry (a request that met a hub mid-restart got a 503): retry.
    if (state.events && !state.refreshRetry) {
      state.refreshRetry = setTimeout(() => {
        state.refreshRetry = null;
        refresh(false).catch(() => {});
      }, 1000);
    }
    throw error;
  }
  if (seq < state.refreshApplied) return;
  state.refreshApplied = seq;
  state.offline = 0;
  $("#offline").hidden = true;
  state.localWorkspaces = ws.workspaces;
  state.localSessions = ss.sessions;
  mergeLists();
  mobile.pruneDrafts(state.sessions.map((s) => s.id));
  // Layouts of workspaces this page has not loaded yet (the first open
  // of one is then instant).
  for (const workspace of state.workspaces) {
    if (!state.confirmed[workspace.id] && !state.layoutLoading.has(workspace.id)) {
      state.layoutLoading.add(workspace.id);
      loadLayout(workspace.id).catch(() => {}).finally(() => state.layoutLoading.delete(workspace.id));
    }
  }
  if (settleRemoved(seq, ws, ss)) return;
  rememberAttached();
  // Viewer counts changed: a pane that became its session's only viewer
  // fits itself; one that no longer is follows the other viewer's size.
  for (const pane of livePanes()) {
    if (pane.term && pane.size && !pane.replaying && pane.claims() === pane.following) pane.claim();
  }
  const snapshot = JSON.stringify([ws, ss, machines.version()]);
  const busy = $("#sidebar").contains(document.activeElement) &&
    document.activeElement.matches("input, select, textarea");
  if (!force && snapshot === state.snapshot) return;
  if (!force && busy) {
    // The sidebar waits for the field; the drawer (outside it) still
    // follows, so it never offers a session that was removed.
    commands.render();
    return;
  }
  state.snapshot = snapshot;
  render();
  // A pane whose session this page had not heard of yet stood empty.
  const tab = !phone() && state.view ? activeTab() : null;
  if (tab && L.panes(tab.root).some((leaf) => leaf.session && !state.panes.has(leaf.id) && sessionById(leaf.session))) renderStage();
  net.defer("usage", () => launcher.rollupsSoon());
}

// Panes whose session was removed (closed in another browser or by the
// CLI, or its workspace was deleted) let go of it -- only on a list
// fetched after the pane attached. The hub closed those panes in the
// layout; this page follows at once. The focused one says what
// happened. Returns true when it re-rendered (or detached).
function settleRemoved(seq, ws, ss) {
  const shown = livePanes().filter((p) => p.sessionId && seq > (p.shownSeq || 0));
  const gone = shown.filter((p) => !sessionById(p.sessionId));
  // A lost link whose session has no terminal any more will never come
  // back: show it gone.
  for (const pane of shown) {
    const session = sessionById(pane.sessionId);
    if (session && !session.live && !pane.socket && !pane.exited) pane.show(session.id, true);
    // Ended here, with no socket left to hear of it, and relaunched
    // since (another stream): attach the new process.
    else if (session && session.live && session.live.epoch && pane.exited && !pane.socket &&
      session.live.epoch !== pane.epoch) {
      pane.show(session.id);
    }
  }
  if (!gone.length) return false;
  state.snapshot = JSON.stringify([ws, ss, machines.version()]);
  const meta = state.attachedMeta;
  const focused = focusedPane();
  const ours = gone.every((p) => state.closing.has(p.sessionId));
  let message = "";
  if (!ours && gone.includes(focused)) {
    message = `${meta ? meta.name : "That session"} was closed elsewhere (another browser or fw hub).`;
  } else if (!ours) {
    const names = gone.filter((p) => !state.closing.has(p.sessionId)).map((p) => p.lastName || "A session");
    message = `${names.join(", ")} ${names.length === 1 ? "was" : "were"} closed elsewhere (another browser or fw hub).`;
  }
  if (state.view && !workspaceById(state.view)) {
    const name = meta ? meta.workspaceName : "This workspace";
    detach(ours ? undefined : `Workspace ${name} was deleted, and ${meta ? meta.name : "its sessions"} with it.`, "Session removed.");
    return true;
  }
  if (phone()) {
    detach(message || undefined, "Session removed.");
    return true;
  }
  const known = new Set(state.sessions.map((s) => s.id));
  for (const id of [state.view]) {
    if (id && state.layouts[id]) L.prune(state.layouts[id], known);
    if (id && state.confirmed[id]) L.prune(state.confirmed[id], known);
  }
  renderStage();
  if (!state.attached && message) {
    // The tab has nothing left: the empty stage says why.
    state.notice = message;
    state.noticeTitle = "Session removed.";
    renderMain();
  } else if (message) {
    commands.toast(message, { timeout: 5000 });
  }
  if (state.view) loadLayout(state.view).then(() => renderStage()).catch(() => {});
  return true;
}

// This hub's workspaces and sessions, then every machine's (machines.js
// keeps a machine's last lists while it is unreachable).
function mergeLists() {
  state.workspaces = [...(state.localWorkspaces || []), ...machines.workspaces()];
  state.sessions = [...(state.localSessions || []), ...machines.sessions()];
}

// A machine's lists or state changed: refresh (this hub's lists are
// cheap; the merge, repaint, and letting go of panes whose session is
// gone all run as for this hub's own changes).
let renderTimer = 0;
function renderSoon() {
  clearTimeout(renderTimer);
  renderTimer = setTimeout(() => refresh(false).catch(() => {}), 30);
}

const STALE_LINK = "The session this link points to no longer exists (it was closed, or its workspace deleted).";

// --- theme -----------------------------------------------------------

// The server answers colour queries (OSC 4/10/11/12) with the dark
// theme (vt.rs default_color; keep the two in sync).
const TERM_THEMES = {
  dark: {
    background: "#0c0d0c",
    foreground: "#e6e0cf",
    cursor: "#ff5a1f",
    cursorAccent: "#0c0d0c",
    selectionBackground: "rgba(255, 90, 31, 0.3)",
    black: "#22241f",
    red: "#ef5a4c",
    green: "#a3c76d",
    yellow: "#e3b341",
    blue: "#6fa3d6",
    magenta: "#c98bd1",
    cyan: "#62c2b5",
    white: "#d9d3c3",
    brightBlack: "#5d5a51",
    brightRed: "#ff7a66",
    brightGreen: "#bfe08a",
    brightYellow: "#f4cd68",
    brightBlue: "#93bff0",
    brightMagenta: "#e0a9e6",
    brightCyan: "#86ddd0",
    brightWhite: "#f6f1e4",
  },
  light: {
    background: "#f2eee4",
    foreground: "#23241f",
    cursor: "#e2470c",
    cursorAccent: "#f2eee4",
    selectionBackground: "rgba(226, 71, 12, 0.22)",
    black: "#23241f",
    red: "#bf3324",
    green: "#4b7419",
    yellow: "#8f6300",
    blue: "#245e9c",
    magenta: "#863890",
    cyan: "#0f7066",
    white: "#c9c2b0",
    brightBlack: "#6f6b60",
    brightRed: "#dc4430",
    brightGreen: "#5b8a22",
    brightYellow: "#a87700",
    brightBlue: "#2f72ba",
    brightMagenta: "#9f48aa",
    brightCyan: "#168a7e",
    brightWhite: "#fffdf7",
  },
};
const THEMES = ["auto", "dark", "light"];
const prefersLight = window.matchMedia("(prefers-color-scheme: light)");
const themePref = () => {
  const kept = settings.get("appearance.theme", "auto");
  return THEMES.includes(kept) ? kept : "auto";
};
let theme = themePref();
const resolvedTheme = () => (theme === "auto" ? (prefersLight.matches ? "light" : "dark") : theme);

// `persist`: a choice made here (saved as the theme setting); false when
// applying the setting itself.
function applyTheme(next, persist = true) {
  theme = next;
  if (persist && next !== themePref()) settings.set("appearance.theme", next);
  document.documentElement.dataset.theme = theme;
  $("#theme-label").textContent = theme;
  $("#theme-toggle").title = `Theme: ${theme} (click for the next)`;
  for (const pane of livePanes()) {
    if (!pane.term) continue;
    pane.term.options.theme = TERM_THEMES[resolvedTheme()];
    pane.term.options.minimumContrastRatio = contrastFloor();
  }
  sendTheme();
}
// The server answers programs' colour queries (OSC 4/10/11/12) for every
// viewer, so it is told this one's palette on attach, theme change, and
// focus: the last-focused viewer's colours win.
const ANSI_KEYS = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
  "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue", "brightMagenta",
  "brightCyan", "brightWhite"];
function themeMessage() {
  const colors = TERM_THEMES[resolvedTheme()];
  return JSON.stringify({
    type: "theme",
    foreground: colors.foreground,
    background: colors.background,
    cursor: colors.cursor,
    ansi: ANSI_KEYS.map((key) => colors[key]),
  });
}
// Every pane on screen (each shows its own session).
function sendTheme() {
  for (const pane of livePanes()) pane.sendTheme();
}
window.addEventListener("focus", () => sendTheme());
// Agent TUIs (Claude Code) paint with 256-colour indexes tuned for dark
// backgrounds -- pale blue, gold -- that vanish on the light theme's
// paper. xterm lifts any cell below this ratio against its background.
const contrastFloor = () => (resolvedTheme() === "light" ? 4.5 : 1);
prefersLight.addEventListener("change", () => applyTheme(theme, false));

// Settings changed (here, on another device, or a device override): put
// them in force.
settings.onChange((ids) => {
  const has = (prefix) => ids.some((id) => id === prefix || id.startsWith(prefix));
  if (has("appearance.theme") && themePref() !== theme) applyTheme(themePref(), false);
  if (has("appearance.density")) document.body.classList.toggle("density-compact", settings.get("appearance.density") === "compact");
  if (has("terminal.transcript_ligatures")) document.body.classList.toggle("ligatures", settings.get("terminal.transcript_ligatures") === true);
  if (has("appearance.term_font") || has("appearance.font_size") || has("terminal.")) applyTermPrefs();
  if (has("keyboard.pane_prefix")) setPrefixOn(settings.get("keyboard.pane_prefix", false) === true);
  if (has("keyboard.key_bar")) mobile.render();
  if (has("notifications.alerts")) notify.renderChip();
});
document.body.classList.toggle("density-compact", settings.get("appearance.density") === "compact");
document.body.classList.toggle("ligatures", settings.get("terminal.transcript_ligatures") === true);

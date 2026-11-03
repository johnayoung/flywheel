// flywheel hub web client: workspaces > sessions in the sidebar, each
// workspace's tabs of split panes (one terminal per pane), dialogs, a
// command palette, and keyboard shortcuts.
// Plain JS, no build step. Talks to the hub's REST API, the per-session
// terminal WebSocket, and the hub-wide events WebSocket that pushes agent
// status (protocol: crates/flywheel-hub/src/server.rs).
"use strict";

(function () {
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

  function agentStatus(session) {
    if (session.kind === "shell" || session.state !== "running" || !session.live) return null;
    const pushed = state.agent[session.id];
    return pushed && AGENT_LOOK[pushed.status] ? pushed : null;
  }

  // The record's own state, kept across hub restarts (a session without a
  // terminal on this hub still shows how it ended); a running agent shows
  // what it is doing instead.
  function status(session) {
    const agent = agentStatus(session);
    if (agent) {
      const look = AGENT_LOOK[agent.status];
      return {
        ...look,
        detail: agent.detail || "",
        title: agent.detail ? `${look.label}: ${agent.detail}` : look.label,
      };
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
  async function rawApi(method, path, body) {
    const init = { method, credentials: "same-origin", headers: {} };
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
    onMachineFlywheel: (machine) => flywheel.reloadMachine(machine),
    signedOut: (why) => window.location.replace("/?signed_out=" + why + window.location.hash),
  });

  // The Commands drawer (commands.js): agents' suggested code blocks.
  const commands = window.FwHubCommands({
    $,
    el,
    icon,
    isAgent,
    api,
    state,
    sessionById,
    workspaceById,
    MOD,
    openSession: (id) => openSession(id),
    flipTo: (id, from) => flipTo(id, from),
    refresh: () => refresh(),
    termSize: () => sizeHint(),
    focusTerminal: () => focusTerminal(),
  });

  // The Flywheel panel (flywheel.js): verified-loop state for the
  // flywheel-adopted folders.
  const flywheel = window.FwHubFlywheel({
    $, el, icon, api, age, span, state, workspaceById, sessionById, folderLabel,
    machinePrefix: (machine) => machines.prefix(machine),
    openSession: (id, place) => openSession(id, place),
    toast: commands.toast,
    confirmDialog: (options) => confirmDialog(options),
    refresh: () => refresh(),
    rerender: () => render(),
    onOpen: () => {
      if (commands.isOpen()) commands.toggle();
      review.close();
    },
    focusTerminal: () => focusTerminal(),
  });

  // The phone and alerts modules (pwa.js, attention.js, notify.js,
  // mobile.js).
  const pwa = window.FwHubPwa();
  const attention = window.FwHubAttention({
    $, el, icon, age, state, workspaceById, agentStatus, openSession: (id) => openSession(id),
    flywheel,
  });
  const notify = window.FwHubNotify({
    $, state, store, sessionById, workspaceById, pwa, api, attention, toast: commands.toast,
    openSession: (id) => openSession(id),
    present: () => present(),
  });
  // Sibling worktrees (worktree.js): the new-session fields, row badges,
  // and the keep / remove close dialog.
  const worktrees = window.FwHubWorktree({ $, el, icon, api });
  // The agent launcher and cost meter (launcher.js): the form's agent
  // fields, launch templates, quick launch, and token usage.
  const launcher = window.FwHubLauncher({
    $, el, api, state, worktrees,
    remoteUsage: () => machines.usage(),
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
  // Review (review.js): a session's commits, diffs, and the guarded
  // commit / push / PR / rebase / merge actions.
  const review = window.FwHubReview({
    $, el, icon, api, state, sessionById, age, MOD,
    toast: commands.toast,
    copyText: (text) => commands.copyText(text),
    confirmDialog: (options) => confirmDialog(options),
    closeSession: (id) => closeSession(id),
    refresh: () => refresh(),
    focusTerminal: () => focusTerminal(),
    onOpen: () => {
      if (commands.isOpen()) commands.toggle();
      flywheel.close();
      explain.close();
    },
  });
  // The "why?" beside the status pill (explain.js).
  const explain = window.FwHubExplain({ $, el, api });
  // Search across sessions (search.js): scrollback, transcripts, commands.
  const search = window.FwHubSearch({
    $, el, icon, api, state, sessionById, workspaceById, age, glyphFor,
    toast: commands.toast,
    openSession: (id) => openSession(id),
    openCommandsAt: (id, seq, all) => {
      review.close();
      flywheel.close();
      commands.openAt(id, seq, all);
    },
    livePanes: () => livePanes(),
    focusTerminal: () => focusTerminal(),
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
    launcher.rollupsSoon();
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
  let theme = THEMES.includes(store.get("theme", "auto")) ? store.get("theme", "auto") : "auto";
  const resolvedTheme = () => (theme === "auto" ? (prefersLight.matches ? "light" : "dark") : theme);

  function applyTheme(next) {
    theme = next;
    store.set("theme", theme);
    document.documentElement.dataset.theme = theme;
    $("#theme-label").textContent = theme;
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
  prefersLight.addEventListener("change", () => applyTheme(theme));

  // --- terminals: panes in tabs and splits ----------------------------
  //
  // Each workspace has a layout (tabs; each a tree of splits whose leaves
  // are panes showing one session), stored on the hub and shared by every
  // browser (layout.js, panes.js). Which tab is active, which pane has
  // focus, and zoom are this browser's. A phone shows one pane at a time
  // (the "solo" pane) and never edits layouts.

  const TERM_FONT = '"FW Term", ui-monospace, "SF Mono", Menlo, Consolas, monospace';
  const L = window.FwHubLayout;
  const phone = () => window.matchMedia(NARROW).matches;
  // Wide enough for an agent and its companion shell side by side.
  const WIDE = "(min-width: 1101px)";

  const focusedPane = () => (phone() ? state.solo : state.panes.get(state.focusId)) || null;
  const focusedTerm = () => {
    const pane = focusedPane();
    return pane && pane.sessionId ? pane.term : null;
  };
  const livePanes = () => (phone() ? (state.solo ? [state.solo] : []) : [...state.panes.values()]);
  // The attached session is the focused pane's.
  Object.defineProperty(state, "attached", {
    get: () => {
      const pane = focusedPane();
      return pane ? pane.sessionId : null;
    },
  });
  Object.defineProperty(state, "exited", { get: () => (focusedPane() ? focusedPane().exited : null) });
  Object.defineProperty(state, "replaying", { get: () => !!(focusedPane() && focusedPane().replaying) });

  const mobile = window.FwHubMobile({
    $, el, state, store, api, sessionById, agentStatus,
    toast: commands.toast,
    copyText: commands.copyText,
    term: () => focusedTerm(),
    pane: () => focusedPane(),
    panes: () => livePanes(),
    phone: () => phone(),
    sendData: (data) => {
      const pane = focusedPane();
      if (pane) pane.sendInput(JSON.stringify({ type: "input", data }));
    },
    onKey: () => {
      const term = focusedTerm();
      if (term) term.scrollToBottom();
    },
  });

  // A pane resizes its session's PTY (fits it to its box) only while it
  // has focus, or while this page is the session's only viewer; else it
  // follows the size another viewer set (panes.js).
  function claimsSize(pane) {
    if (!pane.sessionId) return false;
    if (pane === focusedPane()) return true;
    const session = sessionById(pane.sessionId);
    return !!(session && session.live && (session.live.clients || 0) <= 1);
  }

  const panes = window.FwHubPanes({
    el, state, sessionById, TERM_FONT, status, goneText, exitHow,
    // A machine's session attaches through this hub's proxy.
    socketPath: (id) => machines.route("GET", `/api/sessions/${id}/ws`),
    fontSize: () => mobile.fontSize(),
    termTheme: () => ({ theme: TERM_THEMES[resolvedTheme()], minimumContrastRatio: contrastFloor() }),
    transformInput: (data) => mobile.transformInput(data),
    fitTerminal: (pane) => mobile.fitTerminal(pane),
    attachTerminal: (pane) => mobile.attachTerminal(pane.term),
    themeMessage: () => themeMessage(),
    claims: (pane) => claimsSize(pane),
    // Only dividers still on the stage hold (one removed mid-drag, by a
    // rebuild, never fires its release).
    holdFit: () => [...state.dividerHeld].some((d) => d.isConnected),
    onResize: (pane) => {
      if (pane === focusedPane()) updateMeters();
      renderPaneHead(pane);
    },
    onLink: (pane) => {
      if (pane === focusedPane()) setLink(pane.link.kind, pane.link.text);
    },
    onFocusIn: (pane) => {
      if (!phone() && pane.id !== state.focusId) focusPane(pane.id, { fromTerminal: true });
    },
    onAttached: (pane) => {
      // A re-attach (reconnect, hub restart) never takes the keys from an
      // open drawer, panel, or dialog.
      if (pane === focusedPane() && mayTakeFocus()) focusTerminal();
      refresh(false).catch(() => {});
    },
    onExit: (pane) => {
      if (sessionById(pane.sessionId)) refresh().catch(showError);
      if (pane === focusedPane()) renderMain();
    },
    onSessionGone: () => refresh().catch(() => {}),
    relaunch: (id) => relaunch(id),
    closeSession: (id) => closeSession(id),
    refresh: () => refresh(),
  });

  // --- per-browser view of each workspace's layout ---------------------

  // workspace id -> {tab, focus: {tab id: pane id}, zoom: {tab id: pane id}}
  const views = (() => {
    const kept = store.get("layout-view", {});
    return kept && typeof kept === "object" ? kept : {};
  })();
  function viewOf(workspaceId) {
    if (!views[workspaceId] || typeof views[workspaceId] !== "object") views[workspaceId] = {};
    const view = views[workspaceId];
    if (!view.focus || typeof view.focus !== "object") view.focus = {};
    if (!view.zoom || typeof view.zoom !== "object") view.zoom = {};
    return view;
  }
  const saveViews = () => store.set("layout-view", views);

  const layoutOf = (workspaceId) => state.layouts[workspaceId] || null;
  const viewLayout = () => layoutOf(state.view);

  function activeTab(workspaceId = state.view) {
    const layout = layoutOf(workspaceId);
    if (!layout || !layout.tabs.length) return null;
    const view = viewOf(workspaceId);
    return layout.tabs.find((t) => t.id === view.tab) || layout.tabs[0];
  }

  // The leaf (data, not pane object) that has focus in the active tab.
  function focusedLeaf() {
    const tab = activeTab();
    if (!tab) return null;
    const leaves = L.panes(tab.root);
    return leaves.find((p) => p.id === viewOf(state.view).focus[tab.id]) || leaves[0];
  }

  const zoomedIn = (tab) => {
    if (!tab) return null;
    const id = viewOf(state.view).zoom[tab.id];
    return id && L.findPane(tab.root, id) ? id : null;
  };

  // --- layouts: loading and saving -------------------------------------

  async function loadLayout(workspaceId) {
    const layout = await api("GET", `/api/workspaces/${workspaceId}/layout`);
    const queue = state.layoutQueue[workspaceId];
    // A write in flight settles it (its answer, or a 409 carrying it).
    if (queue && queue.length) return layoutOf(workspaceId);
    // A GET that started before this page's own save but answered after
    // it is older than what this page already has: keep the newer one.
    const have = state.confirmed[workspaceId];
    if (have && layout && typeof layout.rev === "number" && layout.rev < have.rev) return layoutOf(workspaceId);
    state.confirmed[workspaceId] = layout;
    state.layouts[workspaceId] = L.clone(layout);
    return layout;
  }

  async function ensureLayout(workspaceId) {
    if (layoutOf(workspaceId)) return layoutOf(workspaceId);
    try {
      return await loadLayout(workspaceId);
    } catch (error) {
      if (!layoutOf(workspaceId)) state.layouts[workspaceId] = { rev: 0, tabs: [] };
      state.confirmed[workspaceId] = state.confirmed[workspaceId] || { rev: 0, tabs: [] };
      throw error;
    }
  }

  // Change a workspace's layout: `edit(layout)` edits a copy in place (or
  // returns false: nothing to do). It shows at once, then is saved; when
  // another browser saved first (409), it is replayed onto theirs. Edits
  // are saved one at a time, in order.
  function editLayout(workspaceId, edit) {
    const shown = L.clone(layoutOf(workspaceId) || { rev: 0, tabs: [] });
    if (edit(shown) === false) return Promise.resolve(false);
    state.layouts[workspaceId] = shown;
    renderStage();
    const queue = state.layoutQueue[workspaceId] || (state.layoutQueue[workspaceId] = []);
    return new Promise((resolve) => {
      queue.push({ edit, resolve });
      if (queue.length === 1) drainLayout(workspaceId);
    });
  }

  async function drainLayout(workspaceId) {
    const queue = state.layoutQueue[workspaceId];
    while (queue.length) {
      const { edit, resolve } = queue[0];
      let ok = false;
      for (let attempt = 0; attempt < 4; attempt++) {
        const base = L.clone(state.confirmed[workspaceId] || { rev: 0, tabs: [] });
        if (edit(base) === false) {
          ok = true;
          break;
        }
        // Never send a pane for a session this page has not heard of yet.
        L.prune(base, new Set(state.sessions.filter((s) => s.workspace_id === workspaceId).map((s) => s.id)));
        try {
          state.confirmed[workspaceId] = await putLayout(workspaceId, base);
          ok = true;
          break;
        } catch (error) {
          if (error.layout) {
            state.confirmed[workspaceId] = error.layout;
            continue;
          }
          commands.toast(`Layout not saved: ${error.message}`, { tone: "bad", timeout: 6000 });
          break;
        }
      }
      if (!ok) {
        // Gave up: show what the hub has.
        try {
          state.confirmed[workspaceId] = await api("GET", `/api/workspaces/${workspaceId}/layout`);
        } catch (_) {
          // Unreachable: keep the last confirmed one.
        }
      }
      queue.shift();
      resolve(ok);
    }
    // Settled: show the hub's layout plus nothing pending.
    if (state.confirmed[workspaceId]) {
      state.layouts[workspaceId] = L.clone(state.confirmed[workspaceId]);
      if (workspaceId === state.view) renderStage();
    }
  }

  async function putLayout(workspaceId, layout) {
    const response = await fetch(machines.route("PUT", `/api/workspaces/${workspaceId}/layout`), {
      method: "PUT",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ rev: layout.rev, tabs: layout.tabs }),
    });
    if (response.status === 401) {
      window.location.replace("/" + window.location.hash);
      throw new Error("signed out");
    }
    const data = await response.json().catch(() => null);
    if (response.ok) return data;
    const error = new Error((data && data.error) || response.statusText);
    if (response.status === 409 && data && data.layout) error.layout = data.layout;
    throw error;
  }

  // Another browser saved a layout (events socket).
  function onLayoutEvent(message) {
    const id = message.workspace_id;
    const confirmed = state.confirmed[id];
    if (confirmed && confirmed.rev >= message.rev) return;
    if (state.layoutQueue[id] && state.layoutQueue[id].length) return;
    if (!confirmed && id !== state.view) return;
    loadLayout(id).then(() => {
      if (id === state.view) renderStage();
    }).catch(() => {});
  }

  // --- placing sessions ------------------------------------------------

  // Show a session: in the pane already showing it (its tab comes to the
  // front), else -- `place` -- in the focused pane (`{pane}`: that one), a
  // new split beside it (`{split: "right"|"left"|"down"|"up"}`; when
  // either half would be under MIN_PANE, `fallback: "swap"` shows it in
  // the pane instead, else it gets a new tab; `keepZoom` zooms the new
  // pane when the split one was zoomed), or a new tab (`{newTab: true}`).
  // Phones show it alone, the layout untouched.
  async function openSession(id, place = {}) {
    const session = sessionById(id);
    if (!session) return;
    state.notice = "";
    state.noticeTitle = "";
    delete state.wsErrors[session.workspace_id];
    rememberWorkspace(session.workspace_id);
    document.body.classList.remove("side-open");
    const workspaceId = session.workspace_id;
    try {
      await ensureLayout(workspaceId);
    } catch (_) {
      // Unreachable hub: the layout loads with the next refresh.
    }
    if (!sessionById(id)) return;
    state.view = workspaceId;
    if (phone()) {
      showSolo(id);
      return;
    }
    const layout = layoutOf(workspaceId);
    const tab = activeTab();
    const where = [tab, ...layout.tabs.filter((t) => t !== tab)].filter(Boolean)
      .map((t) => ({ tab: t, leaf: L.paneShowing(t.root, id) }))
      .find((hit) => hit.leaf);
    if (where && !place.split && !place.newTab && !place.pane) {
      selectTab(where.tab.id, where.leaf.id);
      return;
    }
    const view = viewOf(workspaceId);
    if (place.newTab) {
      const fresh = L.tab(L.nextTabName(layout), id);
      if (layout.tabs.length >= L.MAX_TABS) {
        commands.toast(`At most ${L.MAX_TABS} tabs per workspace.`, { timeout: 3000 });
        return;
      }
      view.tab = fresh.id;
      view.focus[fresh.id] = fresh.root.id;
      saveViews();
      await editLayout(workspaceId, (draft) => {
        draft.tabs.push(L.clone(fresh));
      });
      return;
    }
    if (!tab) {
      const fresh = L.tab("main", id);
      view.tab = fresh.id;
      view.focus[fresh.id] = fresh.root.id;
      saveViews();
      await editLayout(workspaceId, (draft) => {
        if (draft.tabs.length) {
          // Another browser laid it out meanwhile: show it in its first tab.
          const first = L.panes(draft.tabs[0].root)[0];
          if (!L.sessionsOf(draft.tabs[0].root).includes(id)) first.session = id;
          return;
        }
        draft.tabs.push(L.clone(fresh));
      });
      return;
    }
    const target = place.pane ? L.findPane(tab.root, place.pane) : focusedLeaf();
    if (!target) return;
    if (place.split && !roomToSplit(tab, target.id, place.split, place.ratio || 0.5)) {
      // Too small to halve (an agent at a few columns is unusable): show
      // it in place (`fallback: "swap"`), else in a new tab.
      if (place.fallback === "swap") {
        place = { pane: target.id };
      } else {
        commands.toast(`No room to split (panes keep ${MIN_PANE.cols}x${MIN_PANE.rows}): opened in a new tab.`, { timeout: 4000 });
        await openSession(id, { newTab: true });
        return;
      }
    }
    if (place.split) {
      if (L.panes(tab.root).length >= L.MAX_PANES && !(where && where.tab === tab)) {
        commands.toast(`At most ${L.MAX_PANES} panes per tab.`, { timeout: 3000 });
        return;
      }
      const freshId = L.newId("p");
      view.focus[tab.id] = freshId;
      // A split made from a zoomed pane can stay zoomed, on the new one.
      if (place.keepZoom && zoomedIn(tab)) view.zoom[tab.id] = freshId;
      else delete view.zoom[tab.id];
      saveViews();
      await editLayout(workspaceId, (draft) => {
        const t = draft.tabs.find((x) => x.id === tab.id);
        if (!t || !L.findPane(t.root, target.id)) return false;
        // Shown elsewhere in this tab: it moves.
        const old = L.paneShowing(t.root, id);
        if (old && old.id === target.id) return false;
        if (old) t.root = L.close(t.root, old.id) || t.root;
        const done = L.split(t.root, target.id, place.split, id, place.ratio || 0.5);
        if (!done) return false;
        done.pane.id = freshId;
        t.root = done.root;
      });
      return;
    }
    view.focus[tab.id] = target.id;
    saveViews();
    await editLayout(workspaceId, (draft) => {
      const t = draft.tabs.find((x) => x.id === tab.id);
      const leaf = t && L.findPane(t.root, target.id);
      if (!leaf || leaf.session === id) return false;
      // Already in this tab in another pane: swap the two.
      const other = L.paneShowing(t.root, id);
      if (other) other.session = leaf.session;
      leaf.session = id;
      if (other && !other.session) t.root = L.close(t.root, other.id) || t.root;
    });
  }

  // Bring tab `tabId` of the viewed workspace to the front (this browser
  // only), focusing `paneId` when given.
  function selectTab(tabId, paneId) {
    const view = viewOf(state.view);
    const switching = (activeTab() || {}).id !== tabId;
    view.tab = tabId;
    if (paneId) view.focus[tabId] = paneId;
    saveViews();
    if (switching || !state.panes.size) renderStage();
    const leaf = focusedLeaf();
    if (leaf) focusPane(leaf.id);
  }

  // --- the stage -------------------------------------------------------

  // Build the stage for the viewed workspace: the active tab's tree on a
  // desktop (panes of other tabs are closed: no sockets, no rendering),
  // or the solo pane on a phone.
  function renderStage() {
    const host = $("#terminal");
    if (phone()) {
      for (const [id, pane] of state.panes) {
        pane.dispose();
        state.panes.delete(id);
      }
      state.stageKey = "";
      if (state.solo && state.solo.el.parentNode !== host) host.replaceChildren(state.solo.el);
      if (!state.solo) host.replaceChildren();
      renderTabs();
      renderMain();
      sendView();
      return;
    }
    if (state.solo) {
      state.solo.dispose();
      state.solo = null;
    }
    // Asked before any pane goes: a closed pane takes its focused xterm
    // with it, and the keys typed next must reach the pane that took over.
    const refocus = mayTakeFocus();
    const tab = activeTab();
    const leaves = tab ? L.panes(tab.root) : [];
    for (const [id, pane] of state.panes) {
      if (!leaves.some((leaf) => leaf.id === id && leaf.session)) {
        pane.dispose();
        state.panes.delete(id);
      }
    }
    const leaf = focusedLeaf();
    state.focusId = leaf ? leaf.id : null;
    for (const node of leaves) {
      if (!node.session || !sessionById(node.session)) continue;
      let pane = state.panes.get(node.id);
      if (!pane) {
        pane = panes.create(node.id);
        wirePane(pane);
        state.panes.set(node.id, pane);
      }
      if (pane.sessionId !== node.session) {
        pane.show(node.session);
        pane.shownSeq = state.refreshSeq;
      }
    }
    const zoom = zoomedIn(tab);
    const tree = !tab ? null : zoom ? { type: "pane", id: zoom } : tab.root;
    // Rebuild only when the shape changed (a moved xterm loses focus);
    // a ratio alone is restyled in place.
    const key = JSON.stringify([tab && tab.id, shape(tree)]);
    if (key !== state.stageKey) {
      state.stageKey = key;
      const park = $("#pane-park");
      for (const pane of state.panes.values()) park.append(pane.el);
      host.replaceChildren(...(tree ? [buildNode(tree, "")] : []));
    } else if (tree) {
      restyle(tree, "");
    }
    host.classList.toggle("multi", !!tree && tree.type === "split");
    for (const pane of state.panes.values()) {
      pane.el.classList.toggle("focused", pane.id === state.focusId);
      renderPaneHead(pane);
    }
    if (refocus) focusTerminal();
    // The header's link pill is the focused pane's, whoever changed focus.
    syncLink();
    renderTabs();
    history.replaceState(null, "", state.attached ? `#session=${state.attached}` : window.location.pathname);
    rememberAttached();
    render();
    sendView();
  }

  const shape = (node) => (!node ? null : node.type === "pane"
    ? (node.session === null ? `${node.id}:empty` : node.id)
    : [node.dir, shape(node.a), shape(node.b)]);

  function buildNode(node, path) {
    if (node.type === "pane") {
      const pane = state.panes.get(node.id);
      if (pane) return pane.el;
      return emptyPane(node.id);
    }
    const a = buildNode(node.a, path + "a");
    const b = buildNode(node.b, path + "b");
    const first = el("div", { className: "split-side" }, a);
    const second = el("div", { className: "split-side" }, b);
    const divider = el("div", {
      className: "divider",
      role: "separator",
      tabIndex: 0,
      "aria-orientation": node.dir === "right" ? "vertical" : "horizontal",
      "aria-valuemin": 5,
      "aria-valuemax": 95,
      "aria-label": "Resize panes (arrow keys)",
      title: "Drag to resize (arrow keys when focused; double-click: even)",
      dataset: { path },
    });
    bindDivider(divider, path, node.dir);
    const box = el("div", { className: `split ${node.dir}`, dataset: { path } }, first, divider, second);
    styleSplit(box, node);
    return box;
  }

  function styleSplit(box, node) {
    const [first, divider, second] = box.children;
    first.style.flex = `${node.ratio} 1 0`;
    second.style.flex = `${1 - node.ratio} 1 0`;
    divider.setAttribute("aria-valuenow", String(Math.round(node.ratio * 100)));
  }

  function restyle(node, path) {
    if (node.type !== "split") return;
    const box = $(`#terminal .split[data-path="${path}"]`);
    if (box) styleSplit(box, node);
    restyle(node.a, path + "a");
    restyle(node.b, path + "b");
  }

  // A pane with no session (its session was closed, or a new tab): the
  // empty stage stands in for it when it is the tab's only pane.
  function emptyPane(id) {
    return el("section", { className: "pane empty-pane", dataset: { pane: id } });
  }

  // Dividers: drag (pointer) or arrow keys; saved when the drag ends.
  function bindDivider(divider, path, dir) {
    const ratioAt = (event) => {
      const box = divider.parentElement.getBoundingClientRect();
      const r = dir === "right" ? (event.clientX - box.left) / box.width : (event.clientY - box.top) / box.height;
      return L.clampRatio(r);
    };
    // Restyled only: the layout changes (and is saved) on commit. While a
    // divider is held the panes keep their terminal size (panes.js refit);
    // letting go fits them once.
    const preview = (ratio) => styleSplit(divider.parentElement, { ratio });
    const hold = () => {
      state.dividerHeld.add(divider);
    };
    const release = () => {
      state.dividerHeld.delete(divider);
      for (const d of state.dividerHeld) if (!d.isConnected) state.dividerHeld.delete(d);
      if (!state.dividerHeld.size) {
        requestAnimationFrame(() => {
          for (const pane of livePanes()) pane.refit();
        });
      }
    };
    const commit = (ratio) => {
      const tab = activeTab();
      if (!tab) return;
      editLayout(state.view, (draft) => {
        const t = draft.tabs.find((x) => x.id === tab.id);
        const node = t && L.splitAt(t.root, path);
        if (!node || node.ratio === ratio) return false;
        node.ratio = ratio;
      });
    };
    divider.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      divider.setPointerCapture(event.pointerId);
      divider.classList.add("dragging");
      document.body.classList.add(dir === "right" ? "resizing-x" : "resizing-y");
      hold();
      let last = null;
      const move = (e) => {
        last = ratioAt(e);
        preview(last);
      };
      const up = () => {
        divider.removeEventListener("pointermove", move);
        divider.classList.remove("dragging");
        document.body.classList.remove("resizing-x", "resizing-y");
        if (last !== null) commit(last);
        release();
      };
      divider.addEventListener("pointermove", move);
      divider.addEventListener("pointerup", up, { once: true });
      divider.addEventListener("pointercancel", up, { once: true });
    });
    divider.addEventListener("dblclick", () => {
      preview(0.5);
      commit(0.5);
    });
    divider.addEventListener("keydown", (event) => {
      const tab = activeTab();
      const node = tab && L.splitAt(tab.root, path);
      if (!node) return;
      const back = dir === "right" ? "ArrowLeft" : "ArrowUp";
      const forth = dir === "right" ? "ArrowRight" : "ArrowDown";
      const step = event.shiftKey ? 0.1 : 0.02;
      let next = null;
      const current = divider.pending ?? node.ratio;
      if (event.key === back) next = current - step;
      else if (event.key === forth) next = current + step;
      else if (event.key === "Home") next = L.MIN_RATIO;
      else if (event.key === "End") next = 1 - L.MIN_RATIO;
      else if (event.key === "Escape" || event.key === "Enter") {
        event.preventDefault();
        focusTerminal();
        return;
      }
      if (next === null) return;
      event.preventDefault();
      event.stopPropagation();
      next = L.clampRatio(next);
      if (divider.pending === undefined) hold();
      divider.pending = next;
      preview(next);
      clearTimeout(divider.saveTimer);
      divider.saveTimer = setTimeout(() => {
        divider.pending = undefined;
        commit(next);
        release();
      }, 400);
    });
  }

  // Pointer, menus, and drops on a pane.
  function wirePane(pane) {
    pane.el.addEventListener("pointerdown", () => {
      if (pane.id !== state.focusId && !phone()) focusPane(pane.id, { fromTerminal: true });
    }, true);
    pane.head.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      paneMenu(pane, event.clientX, event.clientY);
    });
    pane.head.addEventListener("dblclick", (event) => {
      if (!event.target.closest("button")) toggleZoom(pane.id);
    });
    bindDrop(pane.el, () => pane.id);
  }

  function renderPaneHead(pane) {
    const session = sessionById(pane.sessionId);
    if (session) pane.lastName = session.name;
    if (!session || phone()) {
      pane.head.replaceChildren();
      return;
    }
    const st = status(session);
    const tab = activeTab();
    const zoomed = tab && zoomedIn(tab) === pane.id;
    const size = pane.term ? `${pane.term.cols}×${pane.term.rows}` : "";
    pane.head.replaceChildren(...[
      el("span", { className: `glyph ${glyphFor(session.kind)}` }, icon(glyphFor(session.kind))),
      el("span", { className: `dot ${st.dot}`, title: st.title || st.label }),
      el("span", { className: "pane-name", textContent: session.name, title: session.cwd }),
      el("span", { className: `pane-state ${st.tone}`, textContent: st.label }),
      pane.following
        ? el("span", {
          className: "pane-follow",
          title: "This pane follows the size another viewer set; focus it to fit the terminal to this pane.",
        }, size, el("span", { className: "long", textContent: " · another viewer's size" }))
        : null,
      el(
        "span",
        { className: "pane-tools" },
        splitTool(iconButton("split-right", `Split right: new shell (${MOD} \\)`, () => splitWithShell(pane, "right"))),
        splitTool(iconButton("split-down", `Split down: new shell (${MOD} -)`, () => splitWithShell(pane, "down"))),
        iconButton(zoomed ? "unzoom" : "zoom", zoomed ? `Unzoom (${MOD} Z)` : `Zoom (${MOD} Z)`, () => toggleZoom(pane.id)),
        iconButton("dots", "Pane menu", (event) => {
          const box = event.currentTarget.getBoundingClientRect();
          paneMenu(pane, box.left, box.bottom + 2);
        }),
        iconButton("x", `Close pane: the session keeps running (${MOD} X)`, () => closePane(pane.id)),
      ),
    ].filter(Boolean));
  }

  function splitTool(button) {
    button.classList.add("split-tool");
    return button;
  }

  // The phone's one pane.
  function showSolo(id) {
    if (!state.solo) {
      state.solo = panes.create("solo");
      state.solo.el.classList.add("solo", "focused");
    }
    const host = $("#terminal");
    if (state.solo.el.parentNode !== host) host.replaceChildren(state.solo.el);
    state.solo.show(id);
    state.solo.shownSeq = state.refreshSeq;
    rememberAttached();
    history.replaceState(null, "", `#session=${id}`);
    renderTabs();
    render();
    sendView();
  }

  // Focus a pane of the active tab (this browser only). It claims the
  // PTY size; the one that had focus may go back to following.
  function focusPane(paneId, { fromTerminal } = {}) {
    const tab = activeTab();
    if (!tab || !L.findPane(tab.root, paneId)) return;
    const before = focusedPane();
    viewOf(state.view).focus[tab.id] = paneId;
    saveViews();
    state.focusId = paneId;
    const zoom = zoomedIn(tab);
    if (zoom && zoom !== paneId) {
      delete viewOf(state.view).zoom[tab.id];
      renderStage();
    }
    for (const pane of state.panes.values()) pane.el.classList.toggle("focused", pane.id === paneId);
    const pane = state.panes.get(paneId);
    if (pane) {
      if (!fromTerminal) focusTerminal();
      pane.claim();
      setLink(pane.link.kind, pane.link.text);
    } else {
      setLink("idle");
    }
    if (before && before !== pane) before.refit();
    history.replaceState(null, "", state.attached ? `#session=${state.attached}` : window.location.pathname);
    rememberAttached();
    render();
    sendView();
  }

  // Focus the pane next to the focused one, by where they are on screen.
  function focusToward(direction) {
    const current = focusedPane();
    if (!current || phone()) return;
    const from = current.el.getBoundingClientRect();
    const cx = from.left + from.width / 2;
    const cy = from.top + from.height / 2;
    let best = null;
    for (const pane of state.panes.values()) {
      if (pane === current || !onStage(pane)) continue;
      const r = pane.el.getBoundingClientRect();
      const ok = direction === "left" ? r.right <= from.left + 2
        : direction === "right" ? r.left >= from.right - 2
          : direction === "up" ? r.bottom <= from.top + 2
            : r.top >= from.bottom - 2;
      if (!ok) continue;
      const dx = r.left + r.width / 2 - cx;
      const dy = r.top + r.height / 2 - cy;
      // Prefer panes in line with this one.
      const along = direction === "left" || direction === "right" ? Math.abs(dx) : Math.abs(dy);
      const across = direction === "left" || direction === "right" ? Math.abs(dy) : Math.abs(dx);
      const score = along + across * 2;
      if (!best || score < best.score) best = { pane, score };
    }
    if (best) focusPane(best.pane.id);
  }

  function cyclePane(delta) {
    const tab = activeTab();
    if (!tab || phone()) return;
    const leaves = L.panes(tab.root).filter((p) => state.panes.has(p.id));
    if (leaves.length < 2) return;
    const at = leaves.findIndex((p) => p.id === state.focusId);
    focusPane(leaves[(at + delta + leaves.length) % leaves.length].id);
  }

  function toggleZoom(paneId = state.focusId) {
    const tab = activeTab();
    if (!tab || !paneId || phone()) return;
    const view = viewOf(state.view);
    if (view.zoom[tab.id] === paneId) delete view.zoom[tab.id];
    else if (L.panes(tab.root).length > 1) view.zoom[tab.id] = paneId;
    else return;
    view.focus[tab.id] = paneId;
    saveViews();
    renderStage();
    const pane = state.panes.get(paneId);
    if (pane) requestAnimationFrame(() => pane.claim());
  }

  // Resize the focused pane from the keyboard: move its nearest divider.
  function nudgePane(direction) {
    const tab = activeTab();
    const paneId = state.focusId;
    if (!tab || !paneId || phone()) return;
    const axis = direction === "left" || direction === "right" ? "right" : "down";
    const step = direction === "left" || direction === "up" ? -0.05 : 0.05;
    editLayout(state.view, (draft) => {
      const t = draft.tabs.find((x) => x.id === tab.id);
      if (!t || !L.resizePane(t.root, paneId, axis, step)) return false;
    });
  }

  // Take a pane out of the tab; its session keeps running. The last pane
  // of a tab closes the tab (an only tab is left empty).
  function closePane(paneId = state.focusId) {
    const tab = activeTab();
    if (!tab || !paneId || phone()) return;
    const view = viewOf(state.view);
    const leaves = L.panes(tab.root);
    const at = leaves.findIndex((p) => p.id === paneId);
    if (at < 0) return;
    if (view.focus[tab.id] === paneId) {
      const next = leaves[at + 1] || leaves[at - 1];
      if (next) view.focus[tab.id] = next.id;
    }
    if (view.zoom[tab.id] === paneId) delete view.zoom[tab.id];
    saveViews();
    const layout = viewLayout();
    if (leaves.length === 1 && layout.tabs.length > 1) {
      closeTab(tab.id);
      return;
    }
    editLayout(state.view, (draft) => {
      const t = draft.tabs.find((x) => x.id === tab.id);
      if (!t || !L.findPane(t.root, paneId)) return false;
      t.root = L.close(t.root, paneId) || { type: "pane", id: paneId, session: null };
    });
  }

  // A new shell beside a pane: an agent's companion (in its directory,
  // worktree included) unless that is on screen already, else a shell in
  // the session's folder.
  async function splitWithShell(pane, side) {
    const session = sessionById(pane && pane.sessionId);
    if (!session) return;
    const tab = activeTab();
    if (tab && L.panes(tab.root).length >= L.MAX_PANES) {
      commands.toast(`At most ${L.MAX_PANES} panes per tab.`, { timeout: 3000 });
      return;
    }
    focusPane(pane.id);
    const agent = isAgent(session);
    const companion = agent ? state.sessions.find((s) => s.companion_of === session.id) : null;
    const companionShown = companion && tab && L.paneShowing(tab.root, companion.id);
    try {
      let created;
      if (agent && !companionShown) {
        created = await api("POST", `/api/sessions/${session.id}/companion`, sizeHint());
      } else {
        const workspace = workspaceById(session.workspace_id);
        const folder = session.worktree ? session.worktree.repo : session.cwd;
        const body = { workspace_id: session.workspace_id, kind: "shell", ...sizeHint() };
        if (workspace && workspace.folders.some((f) => f.path === folder && !f.missing)) body.folder = folder;
        created = await api("POST", "/api/sessions", body);
      }
      await refresh();
      await openSession(created.id || created.session.id, { split: side, pane: pane.id });
    } catch (error) {
      commands.toast(error.message, { tone: "bad", timeout: 6000 });
    }
  }

  // Alt+` between an agent and its companion shell: side by side on a
  // wide screen (palette "Companion shell" turns that off), else in place.
  const companionBeside = () => store.get("companion", "split") !== "swap";
  function flipTo(id, from) {
    const tab = activeTab();
    if (phone() || !companionBeside() || !window.matchMedia(WIDE).matches || !tab) {
      openSession(id);
      return;
    }
    const shown = L.paneShowing(tab.root, id);
    const zoomed = zoomedIn(tab);
    if (shown) {
      if (zoomed && zoomed !== shown.id) {
        // Zoomed: flip the zoom over to it rather than unzoom the tab.
        const view = viewOf(state.view);
        view.zoom[tab.id] = shown.id;
        view.focus[tab.id] = shown.id;
        saveViews();
        renderStage();
        focusTerminal();
        return;
      }
      focusPane(shown.id);
      return;
    }
    const at = L.paneShowing(tab.root, from);
    const target = sessionById(id);
    // The agent goes back to the left of its shell; with no room for that
    // it takes the pane's place, as on a narrow screen.
    const side = target && target.companion_of ? "right" : "left";
    const place = { split: side, fallback: "swap", keepZoom: true };
    openSession(id, at ? { ...place, pane: at.id } : place);
  }

  // Smallest pane a split may leave (tmux-like "no space for new pane"):
  // an agent's TUI, and the hub's own footer reading, need about this.
  const MIN_PANE = { cols: 40, rows: 8 };
  const DIVIDER_PX = 7;

  // The terminal grid a pane box of `w` x `h` px holds, measured from a
  // live pane (cell size, and the title bar and padding around the grid).
  function gridOf(w, h) {
    for (const pane of state.panes.values()) {
      const dims = pane.fit && pane.el.isConnected ? pane.fit.proposeDimensions() : null;
      const box = pane.el.getBoundingClientRect();
      if (!dims || !dims.cols || !dims.rows || !box.width || !pane.host.clientWidth) continue;
      const cellW = pane.host.clientWidth / dims.cols;
      const cellH = pane.host.clientHeight / dims.rows;
      return {
        cols: Math.floor((w - (box.width - pane.host.clientWidth)) / cellW),
        rows: Math.floor((h - (box.height - pane.host.clientHeight)) / cellH),
      };
    }
    // Nothing on screen to measure: the terminal font's usual cell.
    const size = mobile.fontSize();
    return { cols: Math.floor((w - 12) / (size * 0.6)), rows: Math.floor((h - 34) / (size * 1.3)) };
  }

  // Whether splitting pane `paneId` of `tab` toward `side` leaves both
  // halves at least MIN_PANE (laid out unzoomed, as other browsers see it).
  function roomToSplit(tab, paneId, side, ratio = 0.5) {
    const stage = $("#terminal").getBoundingClientRect();
    if (!stage.width || !stage.height) return true;
    const box = L.boxes(tab.root, stage.width, stage.height, DIVIDER_PX).get(paneId);
    if (!box) return true;
    const share = Math.min(ratio, 1 - ratio);
    const across = side === "right" || side === "left";
    const half = across
      ? { w: (box.w - DIVIDER_PX) * share, h: box.h }
      : { w: box.w, h: (box.h - DIVIDER_PX) * share };
    const grid = gridOf(half.w, half.h);
    return grid.cols >= MIN_PANE.cols && grid.rows >= MIN_PANE.rows;
  }

  // The focused terminal's size: a reasonable first size for a new one.
  function sizeHint() {
    const term = focusedTerm();
    return term ? { cols: term.cols, rows: term.rows } : {};
  }

  // --- tabs ------------------------------------------------------------

  function newTab() {
    const layout = viewLayout();
    if (!state.view || !layout || phone()) return;
    if (layout.tabs.length >= L.MAX_TABS) {
      commands.toast(`At most ${L.MAX_TABS} tabs per workspace.`, { timeout: 3000 });
      return;
    }
    const fresh = L.tab(L.nextTabName(layout), null);
    const view = viewOf(state.view);
    view.tab = fresh.id;
    view.focus[fresh.id] = fresh.root.id;
    saveViews();
    editLayout(state.view, (draft) => {
      if (draft.tabs.length >= L.MAX_TABS) return false;
      draft.tabs.push(L.clone(fresh));
    });
  }

  function closeTab(tabId) {
    const layout = viewLayout();
    if (!layout) return;
    const at = layout.tabs.findIndex((t) => t.id === tabId);
    if (at < 0) return;
    const view = viewOf(state.view);
    if ((view.tab || layout.tabs[0].id) === tabId) {
      const next = layout.tabs[at + 1] || layout.tabs[at - 1];
      view.tab = next ? next.id : undefined;
    }
    delete view.focus[tabId];
    delete view.zoom[tabId];
    saveViews();
    editLayout(state.view, (draft) => {
      const i = draft.tabs.findIndex((t) => t.id === tabId);
      if (i < 0) return false;
      draft.tabs.splice(i, 1);
    });
  }

  function cycleTab(delta) {
    const layout = viewLayout();
    const tab = activeTab();
    if (!layout || !tab || layout.tabs.length < 2) return;
    const at = layout.tabs.indexOf(tab);
    selectTab(layout.tabs[(at + delta + layout.tabs.length) % layout.tabs.length].id);
  }

  function renameTab(tabId, name) {
    const clean = name.trim().slice(0, L.MAX_TAB_NAME);
    const layout = viewLayout();
    if (!clean || !layout) return;
    if (layout.tabs.some((t) => t.id !== tabId && t.name.toLowerCase() === clean.toLowerCase())) {
      commands.toast(`A tab is already named ${clean}.`, { timeout: 3000 });
      return;
    }
    editLayout(state.view, (draft) => {
      const t = draft.tabs.find((x) => x.id === tabId);
      if (!t || t.name === clean) return false;
      t.name = clean;
    });
  }

  // Move the focused pane's session to a tab of its own.
  function paneToTab(pane) {
    const tab = activeTab();
    const layout = viewLayout();
    if (!pane || !pane.sessionId || !tab || !layout) return;
    if (layout.tabs.length >= L.MAX_TABS) {
      commands.toast(`At most ${L.MAX_TABS} tabs per workspace.`, { timeout: 3000 });
      return;
    }
    const sid = pane.sessionId;
    const session = sessionById(sid);
    const fresh = L.tab(L.nextTabName(layout, session ? session.name.slice(0, 24) : "tab"), sid);
    const view = viewOf(state.view);
    view.tab = fresh.id;
    view.focus[fresh.id] = fresh.root.id;
    saveViews();
    editLayout(state.view, (draft) => {
      const t = draft.tabs.find((x) => x.id === tab.id);
      if (t && L.panes(t.root).length > 1) t.root = L.close(t.root, pane.id) || t.root;
      draft.tabs.push(L.clone(fresh));
    });
  }

  let tabRenaming = null;
  function renderTabs() {
    const bar = $("#tabs");
    const layout = viewLayout();
    if (phone()) {
      renderSwitcher(bar, layout);
      return;
    }
    if (!state.view || !layout || !layout.tabs.length) {
      bar.hidden = true;
      bar.replaceChildren();
      return;
    }
    const active = activeTab();
    const workspace = workspaceById(state.view);
    const tabs = layout.tabs.map((t, i) => {
      if (tabRenaming === t.id) {
        const input = el("input", {
          className: "tab-rename",
          value: t.name,
          maxLength: L.MAX_TAB_NAME,
          "aria-label": "Tab name",
          onkeydown: (e) => {
            e.stopPropagation();
            if (e.key === "Enter") {
              tabRenaming = null;
              renameTab(t.id, e.target.value);
              renderTabs();
              focusTerminal();
            } else if (e.key === "Escape") {
              tabRenaming = null;
              renderTabs();
              focusTerminal();
            }
          },
          onblur: (e) => {
            if (tabRenaming !== t.id) return;
            tabRenaming = null;
            renameTab(t.id, e.target.value);
            renderTabs();
          },
        });
        requestAnimationFrame(() => {
          input.focus();
          input.select();
        });
        return el("span", { className: "tab renaming" }, input);
      }
      const count = L.sessionsOf(t.root).length;
      const urgent = L.sessionsOf(t.root).map(sessionById).filter(Boolean)
        .map((s) => agentStatus(s)?.status).find((s) => s === "blocked") ||
        L.sessionsOf(t.root).map(sessionById).filter(Boolean).map((s) => agentStatus(s)?.status).find((s) => s === "done");
      return el(
        "span",
        {
          className: `tab${t === active ? " active" : ""}`,
          role: "tab",
          "aria-selected": String(t === active),
          dataset: { tab: t.id },
        },
        el("button", {
          type: "button",
          className: "tab-pick",
          title: `${t.name}: ${count} pane${count === 1 ? "" : "s"}${i < 9 ? ` (Ctrl B ${i + 1} with the prefix on)` : ""}; double-click to rename`,
          // The first click of a double-click on another tab switches to
          // it and rebuilds the bar, so the second lands on a new button:
          // a click's `detail` (2 on the second) starts the rename either way.
          onclick: (e) => {
            if (e.detail >= 2) {
              tabRenaming = t.id;
              renderTabs();
              return;
            }
            selectTab(t.id);
            focusTerminal();
          },
          oncontextmenu: (e) => {
            e.preventDefault();
            tabMenu(t, e.clientX, e.clientY);
          },
          onauxclick: (e) => {
            if (e.button === 1) closeTab(t.id);
          },
        },
        urgent ? el("span", { className: `tab-dot ${urgent}`, title: `an agent here is ${urgent}` }) : null,
        t.name,
        count > 1 ? el("b", { className: "tab-count", textContent: count }) : null),
        layout.tabs.length > 1 ? iconButton("x", `Close tab ${t.name} (its sessions keep running)`, () => closeTab(t.id)) : null,
      );
    });
    bar.hidden = false;
    bar.replaceChildren(...[
      el("span", { className: "tabs-ws", textContent: workspace ? workspace.name : "" }),
      el("div", { className: "tab-list", role: "tablist", "aria-label": "Tabs" }, tabs),
      layout.tabs.length < L.MAX_TABS ? iconButton("plus", `New tab (${MOD} T)`, newTab) : null,
      zoomedIn(active) ? el("button", { type: "button", className: "zoom-pill", onclick: () => toggleZoom(zoomedIn(active)), title: `Unzoom (${MOD} Z)` }, "zoomed") : null,
    ].filter(Boolean));
  }

  // Phones: the sessions laid out in the viewed workspace as one row of
  // chips -- one per session (a session shown in several tabs is one
  // terminal), labelled with the first tab showing it.
  function renderSwitcher(bar, layout) {
    const chips = [];
    let activeChip = null;
    if (layout && state.solo && state.solo.sessionId) {
      const multiTab = layout.tabs.length > 1;
      const seen = new Set();
      for (const t of layout.tabs) {
        for (const leaf of L.panes(t.root)) {
          const session = sessionById(leaf.session);
          if (!session || seen.has(session.id)) continue;
          seen.add(session.id);
          const st = status(session);
          const on = leaf.session === state.solo.sessionId;
          chips.push(el(
            "button",
            {
              type: "button",
              className: `pane-chip${on ? " active" : ""}`,
              "aria-pressed": String(on),
              title: multiTab ? `${session.name} (tab ${t.name})` : session.name,
              onclick: () => {
                const view = viewOf(state.view);
                view.tab = t.id;
                view.focus[t.id] = leaf.id;
                saveViews();
                showSolo(leaf.session);
              },
            },
            el("span", { className: `dot ${st.dot}` }),
            multiTab ? el("small", { textContent: `${t.name}` }) : null,
            session.name,
          ));
          if (on) activeChip = chips[chips.length - 1];
        }
      }
    }
    bar.hidden = chips.length < 2;
    const row = chips.length < 2 ? null : el("div", { className: "pane-switch", role: "toolbar", "aria-label": "Panes" }, chips);
    bar.replaceChildren(...(row ? [row] : []));
    // The chip on screen is in view (the row scrolls sideways).
    if (row && activeChip) {
      requestAnimationFrame(() => {
        const left = activeChip.offsetLeft - row.offsetLeft;
        const right = left + activeChip.offsetWidth;
        if (left < row.scrollLeft) row.scrollLeft = Math.max(0, left - 8);
        else if (right > row.scrollLeft + row.clientWidth) row.scrollLeft = right - row.clientWidth + 8;
      });
    }
  }

  // --- menus -----------------------------------------------------------

  function closeMenu() {
    const menu = $("#ctx-menu");
    if (menu) menu.remove();
  }

  function showMenu(x, y, items) {
    closeMenu();
    const menu = el("div", { id: "ctx-menu", className: "ctx-menu", role: "menu" }, items.map((item) => item.group
      ? el("div", { className: "ctx-group", role: "presentation", textContent: item.group })
      : el("button", {
        type: "button",
        role: "menuitem",
        className: item.danger ? "danger" : "",
        disabled: !!item.disabled,
        onclick: () => {
          closeMenu();
          item.run();
        },
      }, el("span", { textContent: item.label }), item.hint ? el("kbd", { textContent: item.hint }) : null)));
    document.body.append(menu);
    const box = menu.getBoundingClientRect();
    menu.style.left = `${Math.max(4, Math.min(window.innerWidth - box.width - 4, x))}px`;
    menu.style.top = `${Math.max(4, Math.min(window.innerHeight - box.height - 4, y))}px`;
    const first = menu.querySelector("button:not([disabled])");
    if (first) first.focus();
    menu.addEventListener("keydown", (event) => {
      const buttons = [...menu.querySelectorAll("button:not([disabled])")];
      const at = buttons.indexOf(document.activeElement);
      if (event.key === "Escape") {
        event.preventDefault();
        closeMenu();
        focusTerminal();
      } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const next = buttons[(at + (event.key === "ArrowDown" ? 1 : -1) + buttons.length) % buttons.length];
        if (next) next.focus();
      }
      event.stopPropagation();
    });
  }
  document.addEventListener("pointerdown", (event) => {
    if (!event.target.closest("#ctx-menu")) closeMenu();
  }, true);
  window.addEventListener("blur", closeMenu);

  function paneMenu(pane, x, y) {
    const session = sessionById(pane.sessionId);
    const tab = activeTab();
    if (!session || !tab) return;
    focusPane(pane.id);
    const shown = new Set(L.sessionsOf(tab.root));
    const others = state.sessions.filter((s) => s.workspace_id === session.workspace_id && !shown.has(s.id));
    const zoomed = zoomedIn(tab) === pane.id;
    const items = [
      { label: "Split right: new shell", hint: `${MOD} \\`, run: () => splitWithShell(pane, "right") },
      { label: "Split down: new shell", hint: `${MOD} -`, run: () => splitWithShell(pane, "down") },
      { label: zoomed ? "Unzoom" : "Zoom", hint: `${MOD} Z`, run: () => toggleZoom(pane.id), disabled: L.panes(tab.root).length < 2 },
      { label: "Move to a new tab", run: () => paneToTab(pane), disabled: L.panes(tab.root).length < 2 },
      { label: "Close pane (keeps running)", hint: `${MOD} X`, run: () => closePane(pane.id) },
      { label: `Close session ${session.name}`, hint: `${MOD} W`, danger: true, run: () => closeSession(session.id) },
    ];
    if (others.length) {
      items.push({ group: "show beside" });
      for (const other of others.slice(0, 8)) {
        items.push({ label: `${other.name} (right)`, run: () => openSession(other.id, { split: "right", pane: pane.id }) });
      }
      items.push({ group: "show here instead" });
      for (const other of others.slice(0, 8)) {
        items.push({ label: other.name, run: () => openSession(other.id, { pane: pane.id }) });
      }
    }
    showMenu(x, y, items);
  }

  function tabMenu(t, x, y) {
    const layout = viewLayout();
    showMenu(x, y, [
      { label: "Rename", run: () => {
        tabRenaming = t.id;
        renderTabs();
      } },
      { label: "New tab", hint: `${MOD} T`, run: newTab, disabled: layout.tabs.length >= L.MAX_TABS },
      { label: "Close tab (sessions keep running)", danger: true, run: () => closeTab(t.id), disabled: layout.tabs.length < 2 },
    ]);
  }

  // --- drag a session from the sidebar onto a pane ---------------------

  const DRAG_TYPE = "application/x-fw-hub-session";
  let dragged = null; // session id being dragged (dataTransfer is unreadable on dragover)

  // Which part of a pane the pointer is over: an edge (split that way)
  // or the middle (show it here).
  function dropZone(box, event) {
    const x = (event.clientX - box.left) / box.width;
    const y = (event.clientY - box.top) / box.height;
    const edges = [["left", x], ["right", 1 - x], ["up", y], ["down", 1 - y]];
    edges.sort((a, b) => a[1] - b[1]);
    return edges[0][1] < 0.28 ? edges[0][0] : "center";
  }

  function bindDrop(node, paneIdOf) {
    const marker = () => {
      let mark = node.querySelector(":scope > .drop-mark");
      if (!mark) {
        mark = el("div", { className: "drop-mark" });
        node.append(mark);
      }
      return mark;
    };
    const clear = () => {
      const mark = node.querySelector(":scope > .drop-mark");
      if (mark) mark.remove();
    };
    node.addEventListener("dragover", (event) => {
      if (!dragged || phone()) return;
      const session = sessionById(dragged);
      if (!session || session.workspace_id !== state.view) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "move";
      const zone = paneIdOf() && node.classList.contains("pane") && !node.classList.contains("empty-pane")
        ? dropZone(node.getBoundingClientRect(), event)
        : "center";
      marker().dataset.zone = zone;
    });
    node.addEventListener("dragleave", (event) => {
      if (!node.contains(event.relatedTarget)) clear();
    });
    node.addEventListener("drop", (event) => {
      const mark = node.querySelector(":scope > .drop-mark");
      const zone = mark ? mark.dataset.zone : "center";
      clear();
      const id = dragged || event.dataTransfer.getData(DRAG_TYPE);
      dragged = null;
      if (!id || !sessionById(id)) return;
      event.preventDefault();
      const paneId = paneIdOf();
      if (zone === "center") openSession(id, paneId ? { pane: paneId } : {});
      else openSession(id, { split: zone, pane: paneId });
    });
  }
  bindDrop($("#empty"), () => {
    const leaf = focusedLeaf();
    return leaf ? leaf.id : null;
  });
  // A session from another workspace cannot join this layout.
  document.addEventListener("dragend", () => {
    dragged = null;
    document.querySelectorAll(".drop-mark").forEach((m) => m.remove());
  });

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
  });

  // --- events: pushed agent status -------------------------------------

  // Someone is at this page: it is visible and focused, and was touched,
  // typed into, or pointed at in the last IDLE_MS. A tab left focused on a
  // desk monitor while its user is away must not count as watching (it
  // would hold off every push about the session it shows).
  const IDLE_MS = 5 * 60 * 1000;
  let lastInput = Date.now();
  let idleTimer = 0;
  const idle = () => Date.now() - lastInput >= IDLE_MS;
  function present() {
    return document.visibilityState === "visible" && document.hasFocus() && !idle();
  }
  function armIdle() {
    clearTimeout(idleTimer);
    idleTimer = setTimeout(() => sendView(), IDLE_MS + 100);
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
    const session = sessionById(state.attached);
    const focused = session && session.live ? session.id : null;
    const visible = livePanes()
      .filter((p) => p.sessionId && p.sessionId !== focused && onStage(p) && sessionById(p.sessionId)?.live)
      .map((p) => p.sessionId);
    return { session_id: focused, visible };
  }

  function sendView(force) {
    machines.sendView(force);
    const socket = state.events;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    const all = currentView();
    const here = (id) => id && !machines.machineOf(id);
    const view = { session_id: here(all.session_id) ? all.session_id : null, visible: all.visible.filter(here) };
    const key = JSON.stringify(view);
    if (!force && key === state.viewing) return;
    state.viewing = key;
    socket.send(JSON.stringify({ type: "view", ...view }));
  }

  // A status push repaints the sidebar -- except under an inline rename,
  // which must not be torn down mid-typing (it repaints when it ends).
  function renderStatus() {
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
      flywheel.reload();
    };
    socket.onmessage = (event) => {
      let message;
      try {
        message = JSON.parse(event.data);
      } catch (_) {
        return;
      }
      if (message.type === "snapshot") {
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
        refresh(false).catch(() => {});
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
  }

  // The other pushes; `machine`: the machine that sent it (null: here).
  function onPush(message, machine) {
    if (message.type === "commands") {
      commands.onEvent(message);
    } else if (message.type === "usage") {
      launcher.onUsage(message);
    } else if (message.type === "flywheel" || message.type === "flywheel_reset") {
      flywheel.onEvent(message, machine);
    } else if (message.type === "layout") {
      onLayoutEvent(message);
    }
  }

  const eventsWake = () => {
    if (!state.events) connectEvents();
    machines.wake();
  };
  window.addEventListener("online", eventsWake);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") eventsWake();
    sendView();
  });
  window.addEventListener("focus", () => sendView());
  window.addEventListener("blur", () => sendView());

  // Tab title and favicon carry what needs you: blocked and done counts.
  const favicon = {
    link: document.querySelector('link[rel="icon"]'),
    image: null,
    drawn: "",
  };

  // Blocked and done agents (optionally all but one session).
  function attentionCounts(except) {
    let blocked = 0;
    let done = 0;
    for (const session of state.sessions) {
      if (session.id === except) continue;
      const agent = agentStatus(session);
      if (agent && agent.status === "blocked") blocked += 1;
      if (agent && agent.status === "done") done += 1;
    }
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

  // --- session actions -------------------------------------------------

  function rememberWorkspace(id) {
    state.currentWorkspace = id;
    store.set("workspace", id);
  }

  // `place`: where it shows (see openSession); the focused pane by default.
  async function createSession(body, place) {
    const created = await api("POST", "/api/sessions", { ...body, ...sizeHint() });
    await refresh();
    await openSession(created.id, place || {});
    return created;
  }

  // Relaunch in place: same record (name, place in the sidebar), a fresh
  // process and scrollback; every pane showing it re-attaches.
  async function relaunch(id) {
    if (!sessionById(id)) return;
    const showing = livePanes().filter((p) => p.sessionId === id);
    try {
      await api("POST", `/api/sessions/${id}/relaunch`, sizeHint());
      await refresh();
      if (!showing.length) {
        await openSession(id);
        return;
      }
      for (const pane of showing) {
        pane.show(id);
        pane.shownSeq = state.refreshSeq;
      }
      render();
    } catch (error) {
      const pane = showing[0];
      if (pane) pane.showBanner(`Relaunch failed: ${error.message}`, pane.endedActions(id), false);
      else showBanner(`Relaunch failed: ${error.message}`, [], false);
    }
  }

  async function stopSession(id) {
    try {
      await api("POST", `/api/sessions/${id}/kill`);
      await refresh();
    } catch (error) {
      flagError(id, error);
    }
  }

  async function closeSession(id) {
    const session = sessionById(id);
    if (!session) return;
    let query = "";
    // A companion shell shares its agent's worktree: closing it never
    // removes that (the agent's close does).
    const companions = state.sessions.filter((s) => s.companion_of === id);
    if (session.worktree && !session.companion_of) {
      const how = await worktrees.confirmClose(session);
      if (!how) return;
      if (how.remove) query = `?remove_worktree=true${how.force ? "&force=true" : ""}`;
    } else if ((session.state === "running" && session.live) || companions.length) {
      const ok = await confirmDialog({
        kicker: "close session",
        title: `Close ${session.name}?`,
        body: [
          session.live && session.state === "running" ? "Its process" : "",
          session.live && session.state === "running" && session.live.pid ? ` (pid ${session.live.pid})` : "",
          session.live && session.state === "running"
            ? " is hung up and the scrollback is discarded. Every attached browser loses it."
            : "",
          companions.length ? ` Its companion ${companions.map((c) => c.name).join(", ")} closes with it.` : "",
        ].join("").trim(),
        ok: "Kill and close",
      });
      if (!ok) return;
    }
    state.closing.add(id);
    try {
      // Closing an agent also closes its companion shells.
      const closing = companions.map((s) => s.id);
      closing.forEach((c) => state.closing.add(c));
      try {
        await api("DELETE", `/api/sessions/${id}${query}`);
      } finally {
        closing.forEach((c) => setTimeout(() => state.closing.delete(c), 3000));
      }
      // The hub closed the panes that showed them; refresh follows.
      await refresh();
    } catch (error) {
      flagError(id, error);
    } finally {
      state.closing.delete(id);
    }
  }

  // `refocus`: the user finished with Enter/Esc (not by clicking away), so
  // the keyboard goes back to the terminal.
  async function renameSession(id, name, refocus) {
    state.renaming = null;
    const session = sessionById(id);
    try {
      if (session && name.trim() && name.trim() !== session.name) {
        await api("PATCH", `/api/sessions/${id}`, { name });
        await refresh();
      } else {
        render();
      }
    } catch (error) {
      flagError(id, error);
    }
    if (refocus) focusTerminal();
  }

  function focusTerminal() {
    const term = focusedTerm();
    if (term && !document.querySelector("dialog[open]") && !paletteOpen() && !search.isOpen() && !$("#ctx-menu")) term.focus();
  }

  // Something over the terminal that owns the keys: a dialog, the
  // palette, a menu, the Commands drawer, the Flywheel panel, Compose.
  function overlayOpen() {
    return !!document.querySelector("dialog[open]") || paletteOpen() || search.isOpen() || !!$("#ctx-menu") ||
      commands.isOpen() || flywheel.isOpen() || review.isOpen() || !$("#compose").hidden;
  }

  // Whether focus may move to the terminal by itself (an attach, a stage
  // rebuild): not out of an overlay or a field the user is in -- only from
  // the stage, or from nowhere (the page itself, as after a closed pane).
  function mayTakeFocus() {
    if (overlayOpen()) return false;
    const active = document.activeElement;
    if ($("#terminal").contains(active) || $("#pane-park").contains(active)) return true;
    // From nowhere only with a hardware keyboard: on a touch screen focus
    // would raise the on-screen keyboard unasked.
    return (!active || active === document.body) && !window.matchMedia("(pointer: coarse)").matches;
  }

  function startRename(id) {
    const session = sessionById(id);
    if (!session) return;
    state.collapsed.delete(session.workspace_id);
    if (window.matchMedia(NARROW).matches) document.body.classList.add("side-open");
    state.renaming = id;
    render();
    const input = $(`.session[data-id="${id}"] .rename`);
    if (input) {
      input.focus();
      input.select();
    }
  }

  function flagError(sessionId, error) {
    const session = sessionById(sessionId);
    if (session) state.wsErrors[session.workspace_id] = error.message;
    render();
  }

  async function deleteWorkspace(workspace) {
    const sessions = state.sessions.filter((s) => s.workspace_id === workspace.id);
    const running = sessions.filter((s) => s.state === "running");
    const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
    const ok = await confirmDialog({
      kicker: "delete workspace",
      title: `Delete ${workspace.name}?`,
      body: [
        running.length
          ? `${plural(running.length, "session")} still ${running.length === 1 ? "runs" : "run"} here: deleting kills ${running.length === 1 ? "it" : "them"}, and every attached browser loses ${running.length === 1 ? "it" : "them"}.`
          : "",
        sessions.length
          ? `Its ${plural(sessions.length, "session record")} ${sessions.length === 1 ? "goes" : "go"} with it.`
          : "",
        "The folders on disk are not touched.",
      ].filter(Boolean).join(" "),
      ok: running.length ? `Kill ${running.length} and delete` : "Delete",
    });
    if (!ok) return;
    // This browser removes them: no "removed elsewhere" notice.
    sessions.forEach((s) => state.closing.add(s.id));
    try {
      for (const session of running) {
        await api("DELETE", `/api/sessions/${session.id}`);
      }
      await api("DELETE", `/api/workspaces/${workspace.id}`);
      if (state.view === workspace.id) detach();
      delete state.layouts[workspace.id];
      delete state.confirmed[workspace.id];
      delete state.wsErrors[workspace.id];
      await refresh();
    } catch (error) {
      state.wsErrors[workspace.id] = error.message;
      render();
    } finally {
      sessions.forEach((s) => state.closing.delete(s.id));
    }
  }

  // --- sidebar ---------------------------------------------------------

  function renderSession(session, index) {
    const st = status(session);
    const active = state.attached === session.id;
    // In another pane on screen.
    const shown = !active && livePanes().some((p) => p.sessionId === session.id && onStage(p));
    const nameNode = state.renaming === session.id
      ? el("input", {
        className: "rename",
        value: session.name,
        maxLength: 100,
        "aria-label": "Session name",
        onclick: (e) => e.stopPropagation(),
        onkeydown: (e) => {
          e.stopPropagation();
          if (e.key === "Enter") renameSession(session.id, e.target.value, true);
          if (e.key === "Escape") {
            state.renaming = null;
            render();
            focusTerminal();
          }
        },
        onblur: (e) => {
          if (state.renaming === session.id) renameSession(session.id, e.target.value);
        },
      })
      : el("span", { className: "session-name", textContent: session.name, title: session.cwd });
    const actions = el(
      "span",
      { className: "session-actions" },
      iconButton("pen", "Rename (F2)", () => startRename(session.id)),
      session.state === "running" && session.live
        ? iconButton("stop", "Stop: hang up, keep the scrollback", () => stopSession(session.id))
        : iconButton("redo", "Relaunch in place", () => relaunch(session.id)),
      iconButton("x", "Close session", () => closeSession(session.id), true),
    );
    return el(
      "li",
      {
        className: `session${active ? " active" : ""}${shown ? " shown" : ""}${session.live ? "" : " gone"}${st.dot === "blocked" ? " needs-you" : ""}${session.companion_of ? " companion" : ""}`,
        dataset: { id: session.id },
        tabIndex: 0,
        role: "button",
        "aria-current": active ? "true" : undefined,
        title: session.live ? undefined : goneText(session),
        // Drag onto a pane: its edge splits that way, its middle shows it there.
        draggable: !window.matchMedia(NARROW).matches,
        ondragstart: (e) => {
          dragged = session.id;
          e.dataTransfer.setData(DRAG_TYPE, session.id);
          e.dataTransfer.setData("text/plain", session.name);
          e.dataTransfer.effectAllowed = "copyMove";
        },
        onclick: () => openSession(session.id),
        ondblclick: (e) => {
          if (!e.target.closest("button")) startRename(session.id);
        },
        onkeydown: (e) => {
          if (e.target !== e.currentTarget) return;
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            e.currentTarget.click();
          } else if (e.key === "F2") {
            e.preventDefault();
            startRename(session.id);
          } else if (e.key === "Delete") {
            e.preventDefault();
            closeSession(session.id);
          }
        },
      },
      index < 9 ? el("span", { className: "num", textContent: index + 1 }) : null,
      el(
        "span",
        { className: `glyph ${glyphFor(session.kind)}`, title: session.kind || "shell" },
        icon(glyphFor(session.kind)),
      ),
      el("span", { className: `dot ${st.dot}`, title: st.title || st.label }),
      nameNode,
      el(
        "span",
        { className: "session-meta" },
        session.machine ? machineBadge(session.machine) : null,
        el("span", { className: `session-state ${st.tone}`, textContent: st.label, title: st.title || undefined }),
        launcher.rowChip(session),
        actions,
      ),
      // What a blocked agent asks, readable without hover (touch).
      st.dot === "blocked" && st.detail
        ? el("span", { className: "session-detail", textContent: st.detail })
        : null,
      // A companion sits under its agent: the agent's line says it all.
      session.companion_of && sessionById(session.companion_of) ? null : worktrees.badge(session),
    );
  }

  const machineBadge = (name) => el("span", { className: "machine-badge", textContent: name, title: `on machine ${name}` });

  // The workspace's most urgent agent status (idle and unknown stay quiet).
  function rollupChip(workspace) {
    const rolled = state.rollup[workspace.id];
    if (!["blocked", "working", "done"].includes(rolled)) return null;
    const count = state.sessions.filter(
      (s) => s.workspace_id === workspace.id && agentStatus(s)?.status === rolled,
    ).length;
    return el("span", {
      className: `ws-status ${rolled}`,
      textContent: count > 1 ? `${count} ${rolled}` : rolled,
      title: `agents here: ${rolled}`,
    });
  }

  function renderWorkspace(workspace, i, numbering) {
    const sessions = state.sessions.filter((s) => s.workspace_id === workspace.id);
    const live = sessions.filter((s) => s.live && s.state === "running").length;
    const collapsed = state.collapsed.has(workspace.id);
    const attachedHere = sessionById(state.attached)?.workspace_id === workspace.id;
    const toggle = () => {
      if (state.collapsed.has(workspace.id)) state.collapsed.delete(workspace.id);
      else state.collapsed.add(workspace.id);
      store.set("collapsed", [...state.collapsed]);
      render();
    };
    const error = state.wsErrors[workspace.id];
    return el(
      "section",
      {
        className: `workspace${collapsed ? " collapsed" : ""}${attachedHere ? " current" : ""}`,
        style: `--i:${i}`,
        dataset: { id: workspace.id },
      },
      el(
        "div",
        { className: "ws-head-row" },
        el(
          "button",
          { type: "button", className: "ws-head", onclick: toggle, "aria-expanded": String(!collapsed) },
          el("span", { className: "chev" }, icon("chev")),
          el("span", { className: "ws-name", textContent: workspace.name, title: workspace.name }),
          rollupChip(workspace),
          launcher.workspaceChip(workspace.id),
          el(
            "span",
            { className: "ws-count", title: `${live} running of ${sessions.length}` },
            live ? el("b", { textContent: `${live}` }) : null,
            live ? "/" : "",
            `${sessions.length}`,
          ),
        ),
        el(
          "span",
          { className: "ws-tools" },
          iconButton("plus", "New session here", () => openSessionDialog(workspace.id)),
          iconButton("gear", "Edit workspace", () => openWorkspaceDialog(workspace)),
          iconButton("x", "Delete workspace", () => deleteWorkspace(workspace), true),
        ),
      ),
      el(
        "div",
        { className: "ws-body" },
        el(
          "div",
          { className: "ws-repos" },
          workspace.folders.map((f) =>
            flywheel.folderChip(f, el("span", {
              className: f.missing ? "missing" : f.git ? "git" : "plain",
              textContent: folderLabel(f),
              title: f.missing
                ? `${f.path} (missing: moved or deleted?)`
                : f.git ? `${f.path} (git repo)` : `${f.path} (plain folder, no git)`,
            }), workspace.id)),
        ),
        error
          ? el("p", {
            className: "ws-error",
            role: "alert",
            textContent: error,
            onclick: () => {
              delete state.wsErrors[workspace.id];
              render();
            },
          })
          : null,
        sessions.length
          ? el("ul", { className: "sessions" }, sessions.map((s) => renderSession(s, numbering.get(s.id))))
          : el(
            "div",
            { className: "ws-empty" },
            "No sessions. ",
            el("button", {
              type: "button",
              className: "text-btn",
              textContent: "start one",
              onclick: () => openSessionDialog(workspace.id),
            }),
          ),
      ),
    );
  }

  // With machines: this hub's workspaces, then each machine's, each group
  // under a head with its connection state.
  function machineGroups(numbering) {
    let i = 0;
    const group = (name, list, empty) => el(
      "section",
      { className: `machine-group${name ? "" : " local"}`, dataset: { machine: name || "" } },
      machines.header(name),
      list.length
        ? list.map((w) => renderWorkspace(w, i++, numbering))
        : el("div", { className: "ws-empty machine-empty", textContent: empty }),
    );
    const local = state.workspaces.filter((w) => !w.machine);
    return [
      group(null, local, "No workspaces on this machine."),
      ...machines.names().map((name) => {
        const list = state.workspaces.filter((w) => w.machine === name);
        const link = machines.linkOf(name);
        return group(name, list, link === "online" ? "No workspaces there." : link === "connecting" ? "Connecting…" : "Not reachable yet.");
      }),
    ];
  }

  function render() {
    search.refreshFilters();
    const numbering = new Map(attachable().map((s, i) => [s.id, i]));
    const root = $("#workspaces");
    if (machines.any()) {
      root.replaceChildren(...machineGroups(numbering));
    } else if (state.workspaces.length === 0) {
      root.replaceChildren(
        el(
          "div",
          { className: "no-workspaces" },
          "A workspace is a name and the folders (git repos or plain directories) you work on together. Create or import one to start sessions.",
        ),
      );
    } else {
      root.replaceChildren(...state.workspaces.map((w, i) => renderWorkspace(w, i, numbering)));
    }
    // The workspace animation plays once, on first paint only.
    root.querySelectorAll(".workspace").forEach((node) => {
      if (state.painted) node.style.animation = "none";
    });
    state.painted = true;

    const running = state.sessions.filter((s) => s.live && s.state === "running").length;
    const tach = $("#tach");
    tach.textContent = `${pad2(running)} live`;
    tach.classList.toggle("on", running > 0);
    const wheel = $("#wheel");
    wheel.style.setProperty("--spin", running ? `${Math.max(1.1, 9 / running)}s` : "0s");
    wheel.style.setProperty("--spin-state", running ? "running" : "paused");

    attention.render();
    launcher.render();
    if (!tabRenaming) renderTabs();
    for (const pane of livePanes()) renderPaneHead(pane);
    renderMain();
  }

  // The header's agent / companion-shell flip (Alt+`, which phones lack).
  function renderFlip(session) {
    const button = $("#flip-toggle");
    const parent = session && session.companion_of ? sessionById(session.companion_of) : null;
    const agent = isAgent(session);
    button.hidden = !parent && !agent;
    if (button.hidden) return;
    const running = agent ? state.sessions.find((s) => s.companion_of === session.id && s.state === "running") : null;
    button.querySelector("use").setAttribute("href", parent ? `#i-${parent.kind}` : "#i-shell");
    $("#flip-label").textContent = parent ? "agent" : "shell";
    button.title = parent
      ? `Back to agent ${parent.name} (${MOD} \`)`
      : running
        ? `Companion shell ${running.name} (${MOD} \`)`
        : `Open a companion shell in ${session.name}'s directory (${MOD} \`)`;
    button.setAttribute("aria-label", button.title);
  }

  function renderMain() {
    const session = sessionById(state.attached);
    // An empty tab of a workspace that is open (a new tab, its sessions
    // closed): the stage offers that workspace's sessions.
    const emptyTab = !session && !phone() && state.view && workspaceById(state.view) && activeTab() ? focusedLeaf() : null;
    const workspace = session ? workspaceById(session.workspace_id) : emptyTab ? workspaceById(state.view) : null;
    $("#empty").hidden = !!session;
    $("#crumb-workspace").replaceChildren(
      ...(workspace && workspace.machine ? [machineBadge(workspace.machine), " "] : []),
      workspace ? workspace.name : "",
    );
    $("#crumb-session").textContent = session ? session.name : emptyTab ? "Empty pane" : "No session";
    // Without a session the header's session controls mean nothing.
    $("#cmd-toggle").hidden = !session;
    $("#link").hidden = !session;
    const kind = $("#crumb-kind");
    kind.hidden = !session;
    if (session) {
      // A restore that could not bring it back as itself runs a shell.
      kind.textContent = (session.kind || "shell") + (session.fallback_command ? " (as shell)" : "");
      kind.className = `kind-tag ${session.kind || "shell"}`;
    }
    // The LRMs keep a path's slashes in place under the start-ellipsis
    // (direction: rtl) styling.
    $("#meter-cwd").textContent = session ? `\u200e${session.cwd}\u200e` : "";
    $("#meter-cwd").title = session ? session.cwd : "";
    launcher.renderHeader(session);
    renderFlip(session);
    updateMeters();
    const counts = attentionCounts();
    document.title = `${titlePrefix(counts)}${session ? `${session.name} · flywheel hub` : "flywheel hub"}`;
    drawFavicon(counts);
    // The phone's only view of other sessions: a badge on the drawer
    // button for the agents elsewhere that need you.
    const elsewhere = attentionCounts(session ? session.id : null);
    const badge = $("#menu-badge");
    const urgent = elsewhere.blocked ? "blocked" : elsewhere.done ? "done" : null;
    badge.hidden = !urgent;
    if (urgent) {
      badge.className = `menu-badge ${urgent}`;
      badge.textContent = String(elsewhere.blocked || elsewhere.done);
    }
    $(".menu-toggle").setAttribute(
      "aria-label",
      urgent ? `Show sessions (${titlePrefix(elsewhere).slice(1, -2)} elsewhere)` : "Show sessions",
    );
    const agent = session ? agentStatus(session) : null;
    const pill = $("#crumb-status");
    pill.hidden = !agent;
    if (agent) {
      pill.textContent = agent.status;
      pill.className = `agent-pill ${agent.status}`;
      pill.title = agent.detail || "";
    }
    explain.render(session, agent);

    commands.render();
    commands.prefetch();
    flywheel.render();
    review.render();
    mobile.render();

    const actions = $("#empty-actions");
    if (!session && emptyTab && !state.notice) {
      const others = state.sessions.filter((s) => s.workspace_id === workspace.id && s.live);
      $("#empty-title").textContent = "Empty pane.";
      $("#empty-body").textContent = others.length
        ? `Show one of ${workspace.name}'s sessions here, drag one in from the sidebar, or start a shell.`
        : `Start a shell in ${workspace.name}, or drag a session in from the sidebar.`;
      actions.replaceChildren(
        el("button", {
          type: "button",
          className: "slab",
          onclick: () => createSession({ workspace_id: workspace.id, kind: "shell" }, { pane: emptyTab.id })
            .catch((error) => commands.toast(error.message, { timeout: 5000 })),
        }, icon("plus"), "New shell"),
        ...others.slice(0, 8).map((other) => el("button", {
          type: "button",
          className: "slab ghost",
          onclick: () => openSession(other.id, { pane: emptyTab.id }),
        }, other.name)),
      );
    } else if (!session) {
      const hasWorkspace = state.workspaces.length > 0;
      $("#empty-title").textContent = state.noticeTitle ||
        (hasWorkspace ? "Nothing on the wheel." : "Set up a workspace.");
      $("#empty-body").textContent = state.notice || (hasWorkspace
        ? (window.matchMedia(NARROW).matches
          ? "Open the menu for every session, or launch a shell, Claude Code, or Codex in one of your folders."
          : "Pick a session from the left, or launch a shell, Claude Code, or Codex in one of your folders.")
        : "Group the folders you work on together (or import a VS Code workspace), then launch shells and agents inside them. Every terminal lives on the hub: close the tab, come back from your phone, it is still there.");
      actions.replaceChildren(
        ...[hasWorkspace
          ? el("button", { type: "button", className: "slab", onclick: () => openSessionDialog() }, icon("plus"), "New session")
          : null,
        el(
          "button",
          { type: "button", className: `slab${hasWorkspace ? " ghost" : ""}`, onclick: () => openWorkspaceDialog() },
          icon("plus"),
          "New workspace",
        )].filter(Boolean),
      );
    }
  }

  function updateMeters() {
    const session = sessionById(state.attached);
    const term = focusedTerm();
    $("#meter-size").textContent = session && term ? `${term.cols}×${term.rows}` : "";
    const viewers = session && session.live ? session.live.clients : 0;
    $("#meter-viewers").textContent = viewers > 1 ? `${viewers} viewers` : "";
  }

  function showError(error) {
    showBanner(`Error: ${error.message}`, [], false);
  }

  // --- dialogs: confirm ------------------------------------------------

  function confirmDialog({ kicker, title, body, ok }) {
    const dialog = $("#dlg-confirm");
    $("#confirm-kicker").textContent = kicker || "confirm";
    $("#confirm-title").textContent = title;
    $("#confirm-body").textContent = body;
    $("#confirm-ok").textContent = ok || "Confirm";
    dialog.returnValue = "";
    dialog.showModal();
    $("#confirm-ok").focus();
    return new Promise((resolve) => {
      dialog.addEventListener("close", () => resolve(dialog.returnValue === "ok"), { once: true });
    });
  }

  // Close buttons inside dialogs, and focus return to the terminal.
  document.querySelectorAll("dialog").forEach((dialog) => {
    dialog.addEventListener("click", (event) => {
      if (event.target.closest("[data-close]")) dialog.close("cancel");
      // A click on the backdrop lands on the dialog element itself.
      if (event.target === dialog) dialog.close("cancel");
    });
    dialog.addEventListener("close", () => focusTerminal());
  });

  // --- dialogs: workspace ----------------------------------------------

  const wsForm = $("#workspace-form");
  // "Import VS Code workspace" (import.js), inside this dialog.
  const importer = window.FwHubImport({ $, el, api, refresh, rememberWorkspace, basename, workspaceById });
  let wsEditing = null; // workspace being edited, or null for create
  let checkSeq = 0;

  // `value`: a path, or a workspace folder {path, name}.
  function repoRow(value) {
    const folder = typeof value === "object" && value ? value : { path: value || "" };
    const input = el("input", {
      type: "text",
      value: folder.path || "",
      placeholder: "~/code/project",
      autocomplete: "off",
      spellcheck: false,
      "aria-label": "Folder path",
    });
    const label = el("input", {
      type: "text",
      className: "folder-label",
      value: folder.name || "",
      placeholder: "label",
      maxLength: 100,
      autocomplete: "off",
      spellcheck: false,
      "aria-label": "Folder label (optional; shown instead of the directory name)",
      title: "Label (optional): shown instead of the directory name",
    });
    const verdict = icon("check");
    verdict.classList.add("verdict");
    const note = el("span", { className: "note" });
    const row = el(
      "li",
      { className: "repo-row", dataset: { state: "empty" } },
      el("span", { className: "input-wrap" }, input, verdict),
      label,
      iconButton("x", "Remove folder", () => {
        row.remove();
        if (!$("#repo-rows").children.length) addRepoRow("");
        markDuplicates();
      }),
      note,
    );
    let timer = 0;
    row.check = () => checkRow(row);
    input.addEventListener("input", () => {
      clearTimeout(timer);
      row.dataset.state = input.value.trim() ? "checking" : "empty";
      setVerdict(row, true);
      // Drop the previous check too: its answer is for the old text.
      row.result = null;
      row.pending = null;
      row.seq = ++checkSeq;
      note.textContent = "";
      timer = setTimeout(row.check, 280);
    });
    input.addEventListener("paste", (event) => {
      const text = (event.clipboardData || window.clipboardData).getData("text");
      const lines = text.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
      if (lines.length < 2) return;
      event.preventDefault();
      input.value = lines[0];
      input.dispatchEvent(new Event("input"));
      let after = row;
      for (const line of lines.slice(1)) {
        const next = repoRow(line);
        after.after(next);
        after = next;
        next.check();
      }
    });
    row.input = input;
    row.label = label;
    row.note = note;
    if (folder.path) {
      row.dataset.state = "checking";
      row.check();
    }
    return row;
  }

  function addRepoRow(value) {
    const row = repoRow(value);
    $("#repo-rows").append(row);
    return row;
  }

  async function checkRow(row) {
    const raw = row.input.value.trim();
    if (!raw) {
      row.dataset.state = "empty";
      row.result = null;
      return null;
    }
    const seq = ++checkSeq;
    row.seq = seq;
    row.dataset.state = "checking";
    row.pendingRaw = raw;
    row.pending = api("GET", `/api/folders/check?path=${encodeURIComponent(raw)}`)
      .then((result) => {
        if (row.seq !== seq) return row.pending;
        // A folder the workspace already has may stay while it is gone
        // from disk (the hub keeps it, flagged missing).
        const kept = wsEditing && wsEditing.folders.find((f) => f.path === raw);
        if (!result.ok && kept && kept.missing) {
          result = { ok: true, path: raw, git: false, missing: true };
        }
        row.result = { ...result, raw };
        row.dataset.state = result.ok ? "ok" : "bad";
        setVerdict(row, result.ok);
        row.note.textContent = result.ok ? okNote(result, raw) : result.error;
        markDuplicates();
        return result;
      })
      .catch((error) => {
        if (row.seq !== seq) return row.pending;
        row.result = { ok: false, error: error.message, raw };
        row.dataset.state = "bad";
        setVerdict(row, false);
        row.note.textContent = error.message;
        return row.result;
      });
    return row.pending;
  }

  // The check answer for exactly what the row says now: the settled
  // result, the in-flight check, or a fresh one -- never an answer for
  // text the user has since changed.
  function currentCheck(row) {
    const raw = row.input.value.trim();
    if (row.result && row.result.raw === raw) return row.result;
    if (row.pending && row.pendingRaw === raw) return row.pending;
    return row.check();
  }

  // A good path's note: where it resolved to, and whether it is a plain
  // folder (repo-only features such as worktrees will not apply).
  function okNote(result, raw) {
    if (result.missing) return "missing on disk (moved or deleted?); kept until you remove it";
    const where = result.path !== raw ? result.path : "";
    if (result.git !== false) return where;
    return where ? `${where} · plain folder (no git)` : "plain folder (no git)";
  }

  function setVerdict(row, ok) {
    row.querySelector(".verdict use").setAttribute("href", ok ? "#i-check" : "#i-x");
  }

  function markDuplicates() {
    const seen = new Map();
    [...$("#repo-rows").children].forEach((row, i) => {
      if (!row.result || !row.result.ok) return;
      const first = seen.get(row.result.path);
      if (first !== undefined) {
        row.note.textContent = `same folder as ${pad2(first + 1)}; listed once`;
      } else {
        seen.set(row.result.path, i);
        if (row.note.textContent.startsWith("same folder as")) {
          row.note.textContent = okNote(row.result, row.input.value.trim());
        }
      }
    });
  }

  function openWorkspaceDialog(workspace) {
    wsEditing = workspace || null;
    wsForm.reset();
    $("#workspace-error").textContent = "";
    $("#dlg-workspace-title").textContent = workspace ? `Edit ${workspace.name}` : "New workspace";
    $("#workspace-kicker").textContent = workspace ? "edit workspace" : "workspace";
    $("#workspace-submit").textContent = workspace ? "Save" : "Create";
    wsForm.name.value = workspace ? workspace.name : "";
    $("#repo-rows").replaceChildren();
    // Which machine it lives on (its folders are paths there).
    const field = $("#ws-machine-field");
    field.hidden = !machines.any();
    const select = $("#ws-machine");
    const current = workspace ? workspace.machine || "" : (machineOfView() || "");
    select.replaceChildren(
      el("option", { value: "", textContent: `${machines.host() || "this machine"} (this hub)` }),
      ...machines.names().map((name) => el("option", { value: name, textContent: name })),
    );
    select.value = current;
    select.disabled = !!workspace;
    if (workspace) workspace.folders.forEach((f) => addRepoRow(f));
    else addRepoRow("");
    importer.reset(!workspace);
    // VS Code import reads this machine's files: here only.
    $("#ws-import").hidden = !!workspace || !!select.value;
    $("#dlg-workspace").showModal();
    wsForm.name.focus();
  }

  $("#add-repo").addEventListener("click", () => addRepoRow("").input.focus());
  // Another machine: the folder paths are checked again, there.
  $("#ws-machine").addEventListener("change", () => {
    $("#ws-import").hidden = !!$("#ws-machine").value;
    for (const row of $("#repo-rows").children) if (row.input.value.trim()) checkRow(row);
  });

  // The machine of the workspace on the stage (a new workspace defaults to it).
  const machineOfView = () => (workspaceById(state.view) || {}).machine || null;

  // Calls without an id (kinds, folder checks, worktree plans, a new
  // workspace) go to the machine the open dialog is about.
  machines.setContext(() => {
    if ($("#dlg-workspace").open) return wsEditing ? wsEditing.machine || null : $("#ws-machine").value || null;
    if ($("#dlg-session").open) return (workspaceById($("#session-workspace").value) || {}).machine || null;
    return null;
  });
  // A stale error (name taken, bad path) clears as soon as the form changes.
  wsForm.addEventListener("input", () => {
    $("#workspace-error").textContent = "";
  });

  wsForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const error = $("#workspace-error");
    error.textContent = "";
    const name = wsForm.name.value.trim();
    if (!name) {
      error.textContent = "Give the workspace a name.";
      wsForm.name.focus();
      return;
    }
    const rows = [...$("#repo-rows").children].filter((row) => row.input.value.trim());
    if (!rows.length) {
      error.textContent = "Add at least one folder.";
      $("#repo-rows .repo-row input").focus();
      return;
    }
    const submit = $("#workspace-submit");
    submit.disabled = true;
    try {
      const results = await Promise.all(rows.map(currentCheck));
      const bad = rows.find((row, i) => !results[i] || !results[i].ok);
      if (bad) {
        error.textContent = "Fix the highlighted folder paths.";
        bad.input.focus();
        return;
      }
      // An empty label clears it (the directory name shows).
      const folders = results.map((r, i) => ({ path: r.path, name: rows[i].label.value.trim() }));
      const saved = wsEditing
        ? await api("PATCH", `/api/workspaces/${wsEditing.id}`, { name, folders })
        : await api("POST", "/api/workspaces", { name, folders });
      rememberWorkspace(saved.id);
      $("#dlg-workspace").close("ok");
      await refresh();
    } catch (e) {
      error.textContent = e.message;
    } finally {
      submit.disabled = false;
    }
  });

  // --- dialogs: session ------------------------------------------------

  const sessionForm = $("#session-form");

  // What the dialog's machine can launch (each machine has its own).
  const kindsKey = () => ($("#dlg-session").open ? (workspaceById($("#session-workspace").value) || {}).machine : null) || "";
  async function loadKinds() {
    const key = kindsKey();
    state.kindsBy = state.kindsBy || {};
    try {
      state.kindsBy[key] = (await api("GET", "/api/kinds")).kinds;
    } catch (_) {
      state.kindsBy[key] = state.kindsBy[key] || [{ kind: "shell", available: true, program: null }];
    }
    if (!key) state.kinds = state.kindsBy[key];
    return state.kindsBy[key];
  }

  function selectedKind() {
    const checked = sessionForm.querySelector('input[name="kind"]:checked');
    return checked ? checked.value : "shell";
  }

  function renderKinds(kinds, preferred) {
    const usable = kinds.filter((k) => k.available).map((k) => k.kind);
    const pick = usable.includes(preferred) ? preferred : usable[0] || "shell";
    const cards = kinds.map((k) =>
      el(
        "label",
        { className: `kind-card ${k.kind}` },
        el("input", {
          type: "radio",
          name: "kind",
          value: k.kind,
          checked: k.kind === pick,
          disabled: !k.available,
          onchange: updateSessionPlaceholder,
        }),
        icon(glyphFor(k.kind)),
        el("span", { className: "kind-name", textContent: k.kind === "claude" ? "Claude" : k.kind === "codex" ? "Codex" : "Shell" }),
        el("span", {
          className: "kind-prog",
          textContent: k.available ? `\u200e${k.program || k.kind}\u200e` : "not installed",
          title: k.available ? (k.program || "") : `${k.kind} is not on the hub's PATH`,
        }),
      ));
    $("#kind-picker").replaceChildren(el("legend", { className: "field-label", textContent: "Runs" }), ...cards);
    launcher.setKind(selectedKind());
  }
  $("#kind-picker").addEventListener("change", () => launcher.setKind(selectedKind()));

  function renderRepoPicker() {
    const workspace = workspaceById($("#session-workspace").value);
    const picker = $("#repo-picker");
    const legend = el("legend", { className: "field-label", textContent: "In folder" });
    if (!workspace) {
      picker.replaceChildren(legend);
      return;
    }
    // The folder of the session you are in, then the last one picked here.
    const attached = sessionById(state.attached);
    const paths = workspace.folders.map((f) => f.path);
    const chosen = [
      attached && attached.workspace_id === workspace.id ? (attached.worktree ? attached.worktree.repo : attached.cwd) : null,
      state.repoChoice[workspace.id],
    ].find((path) => path && paths.includes(path)) || paths[0];
    // Missing folders cannot be opened; they stay listed, disabled.
    const usable = workspace.folders.filter((f) => !f.missing).map((f) => f.path);
    const pick = usable.includes(chosen) ? chosen : usable[0];
    const choices = workspace.folders.map((folder) => {
      const tag = folder.missing ? "missing" : folder.git ? null : "no git";
      const choice = el(
        "label",
        { className: `repo-choice${folder.missing ? " missing" : ""}`, title: folder.path },
        el("input", {
          type: "radio",
          name: "repo",
          value: folder.path,
          checked: folder.path === pick,
          disabled: !!folder.missing,
        }),
        el("b", {}, folderLabel(folder), tag ? el("span", { className: "folder-tag", textContent: tag }) : null),
        el("small", { textContent: folder.path }),
      );
      choice.dataset.match = `${folderLabel(folder)} ${folder.path}`.toLowerCase();
      return choice;
    });
    const list = el("div", { className: "repo-list" }, choices);
    // Many folders (an imported workspace): a filter, and the list scrolls
    // so the Name field and Launch stay in view.
    const filter = workspace.folders.length > 5
      ? el("input", {
        type: "search",
        className: "repo-filter",
        placeholder: `filter ${workspace.folders.length} folders`,
        autocomplete: "off",
        spellcheck: false,
        "aria-label": "Filter folders",
        oninput: (event) => {
          const q = event.target.value.trim().toLowerCase();
          let first = null;
          for (const choice of choices) {
            const shown = !q || choice.dataset.match.includes(q);
            choice.hidden = !shown;
            const radio = choice.querySelector("input");
            if (shown && !radio.disabled && !first) first = radio;
          }
          // Keep the pick among what is shown.
          const checked = choices.find((c) => c.querySelector("input").checked);
          if (first && (!checked || checked.hidden)) first.checked = true;
        },
        onkeydown: (event) => {
          // Enter launches (the form submits); it must not clear the filter.
          if (event.key === "Escape" && event.target.value) {
            event.preventDefault();
            event.stopPropagation();
            event.target.value = "";
            event.target.dispatchEvent(new Event("input"));
          }
        },
      })
      : null;
    list.classList.toggle("long", !!filter);
    picker.replaceChildren(legend, ...(filter ? [filter] : []), list);
    pickedFolderChanged();
  }

  // The worktree fields follow the picked folder (git ones only).
  function pickedFolderChanged() {
    const workspace = workspaceById($("#session-workspace").value);
    const checked = sessionForm.querySelector('input[name="repo"]:checked');
    const folder = workspace && checked ? workspace.folders.find((f) => f.path === checked.value) : null;
    worktrees.setFolder(folder || null);
    updateSessionPlaceholder();
  }

  function updateSessionPlaceholder() {
    const kind = selectedKind();
    const workspaceId = $("#session-workspace").value;
    const count = state.sessions.filter((s) => s.workspace_id === workspaceId).length;
    $("#session-name").placeholder = worktrees.taskName() || `${kind} ${count + 1}`;
  }

  // `place`: preset "Show in" ("pane", "right", "down", "tab").
  async function openSessionDialog(workspaceId, place) {
    if (!state.workspaces.length) {
      openWorkspaceDialog();
      return;
    }
    const target = workspaceById(workspaceId) ||
      workspaceById(sessionById(state.attached)?.workspace_id) ||
      workspaceById(state.currentWorkspace) ||
      state.workspaces[0];
    sessionForm.reset();
    worktrees.reset();
    $("#session-error").textContent = "";
    $("#session-workspace").replaceChildren(
      ...state.workspaces.map((w) => el("option", {
        value: w.id,
        textContent: w.machine ? `${w.name} (${w.machine})` : w.name,
        selected: w.id === target.id,
      })),
    );
    launcher.reset(target.id);
    renderKinds(
      (state.kindsBy && state.kindsBy[target.machine || ""]) || state.kinds || [{ kind: "shell", available: true, program: null }],
      store.get("kind", "shell"),
    );
    renderRepoPicker();
    $("#session-place").value = place || "pane";
    placePickChanged();
    $("#dlg-session").showModal();
    const first = sessionForm.querySelector('input[name="kind"]:checked');
    if (first) first.focus();
    const kinds = await loadKinds();
    if ($("#dlg-session").open) {
      const keep = selectedKind();
      renderKinds(kinds, keep);
      updateSessionPlaceholder();
      const checked = sessionForm.querySelector('input[name="kind"]:checked');
      if (checked && sessionForm.contains(document.activeElement) === false) checked.focus();
    }
  }

  // "Show in" means something only for the workspace on the stage, on a
  // screen with splits; another workspace's session opens in its layout.
  function placePickChanged() {
    const usable = !phone() && !!state.view && $("#session-workspace").value === state.view &&
      !!activeTab() && !!focusedPane();
    $("#place-pick").hidden = !usable;
  }
  $("#session-workspace").addEventListener("change", () => {
    launcher.reset($("#session-workspace").value);
    renderRepoPicker();
    placePickChanged();
    // Another machine launches what is installed there.
    const keep = selectedKind();
    loadKinds().then((kinds) => {
      if ($("#dlg-session").open) renderKinds(kinds, keep);
    });
  });
  $("#repo-picker").addEventListener("change", pickedFolderChanged);
  $("#repo-picker").addEventListener("input", (event) => {
    if (event.target.matches(".repo-filter")) pickedFolderChanged();
  });
  $("#wt-pick").addEventListener("input", updateSessionPlaceholder);
  // Enter launches from anywhere in the form (radios do not submit on
  // their own).
  // In the prompt, Enter is a new line and Ctrl/Cmd Enter launches.
  sessionForm.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" || event.isComposing || event.target.matches("button, select")) return;
    if (event.target.matches("textarea") && !(event.ctrlKey || event.metaKey)) return;
    if (event.target.closest("#tpl-save")) return;
    event.preventDefault();
    sessionForm.requestSubmit();
  });

  sessionForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    // Enter submits too: the live worktree check's error stands.
    if (worktrees.blocked()) return;
    const workspaceId = $("#session-workspace").value;
    const repoInput = sessionForm.querySelector('input[name="repo"]:checked');
    const kind = selectedKind();
    const name = $("#session-name").value.trim();
    const submit = $("#session-submit");
    submit.disabled = true;
    $("#session-error").textContent = "";
    try {
      const body = { workspace_id: workspaceId, kind, ...launcher.request() };
      if (repoInput) body.repo = repoInput.value;
      const worktree = worktrees.request();
      if (worktree) body.worktree = worktree;
      if (name) body.name = name;
      if (repoInput) {
        state.repoChoice[workspaceId] = repoInput.value;
        store.set("repo-choice", state.repoChoice);
      }
      store.set("kind", kind);
      const pick = $("#place-pick").hidden ? "pane" : $("#session-place").value;
      const place = pick === "tab" ? { newTab: true } : pick === "right" || pick === "down" ? { split: pick } : {};
      $("#dlg-session").close("ok");
      await createSession(body, place);
    } catch (error) {
      if (!$("#dlg-session").open) $("#dlg-session").showModal();
      $("#session-error").textContent = error.message;
    } finally {
      submit.disabled = worktrees.blocked();
    }
  });

  // --- command palette -------------------------------------------------

  const palette = {
    items: [],
    shown: [],
    index: 0,
  };

  function paletteCommands() {
    const attached = sessionById(state.attached);
    const items = [];
    const numbering = new Map(attachable().map((s, i) => [s.id, i]));
    for (const session of orderedSessions()) {
      const workspace = workspaceById(session.workspace_id);
      const n = numbering.get(session.id);
      items.push({
        group: "Sessions",
        glyph: glyphFor(session.kind),
        label: session.name,
        hint: `${session.machine ? `${session.machine} · ` : ""}${workspace ? workspace.name : ""} · ${status(session).label}${n < 9 ? ` · ${MOD} ${n + 1}` : ""}${session.live ? "" : " · no terminal"}`,
        run: () => openSession(session.id),
      });
    }
    const cmd = (label, hint, run, glyph = "chev", extra = {}) => items.push({ group: "Commands", glyph, label, hint, run, ...extra });
    cmd("New session", `${MOD} N`, () => openSessionDialog(), "plus");
    for (const workspace of state.workspaces) {
      cmd(`New session in ${workspace.name}`, workspace.machine ? `on ${workspace.machine}` : "", () => openSessionDialog(workspace.id), "plus");
    }
    cmd("New workspace", `${MOD} Shift N`, () => openWorkspaceDialog(), "plus");
    cmd("Machines", "other machines' hubs: add, test, remove", () => machines.openDialog(), "chev");
    for (const name of machines.names()) {
      if (machines.linkOf(name) !== "online") cmd(`Reconnect to ${name}`, machines.linkOf(name), () => machines.reconnect(name), "redo");
    }
    cmd("Import VS Code workspace", ".code-workspace files", () => {
      openWorkspaceDialog();
      importer.open();
    }, "import");
    if (attached) {
      cmd(`Rename ${attached.name}`, `${MOD} R`, () => startRename(attached.id), "pen");
      if (attached.state === "running" && attached.live) {
        cmd(`Stop ${attached.name}`, "keep scrollback", () => stopSession(attached.id), "stop");
      } else {
        cmd(`Relaunch ${attached.name}`, "same session, fresh process", () => relaunch(attached.id), "redo");
      }
      cmd(`Close ${attached.name}`, `${MOD} W`, () => closeSession(attached.id), "x");
      cmd("Detach", "close every pane here; the sessions keep running", () => detach(), "chev");
    }
    if (state.view && !phone()) {
      const tab = activeTab();
      const pane = focusedPane();
      if (pane && pane.sessionId) {
        cmd("Split right: new shell", `${MOD} \\`, () => splitWithShell(pane, "right"), "split-right");
        cmd("Split down: new shell", `${MOD} -`, () => splitWithShell(pane, "down"), "split-down");
        if (tab && L.panes(tab.root).length > 1) {
          cmd(zoomedIn(tab) ? "Unzoom pane" : "Zoom pane", `${MOD} Z`, () => toggleZoom(), "zoom");
          cmd("Move pane to a new tab", "", () => paneToTab(pane), "chev");
        }
        cmd("Close pane", `${MOD} X · the session keeps running`, () => closePane(), "x");
      }
      cmd("New tab", `${MOD} T`, () => newTab(), "plus");
      const layout = viewLayout();
      if (layout) {
        layout.tabs.forEach((t, i) => {
          if (t !== tab) cmd(`Tab: ${t.name}`, `${L.panes(t.root).length} panes${prefix.on && i < 9 ? ` · Ctrl B ${i + 1}` : ""}`, () => selectTab(t.id), "chev");
        });
        if (tab) {
          cmd(`Rename tab ${tab.name}`, prefix.on ? "Ctrl B ," : "double-click the tab", () => {
            tabRenaming = tab.id;
            renderTabs();
          }, "pen");
        }
      }
    }
    cmd(`Pane prefix Ctrl B: turn ${prefix.on ? "off" : "on"}`, prefix.on ? "now on (Ctrl B twice sends ^B)" : "tmux-style: Ctrl B then % \" arrows z x c n p", () => {
      prefix.on = !prefix.on;
      store.set("prefix", prefix.on);
      armPrefix(false);
      commands.toast(prefix.on ? "Pane prefix on: Ctrl B, then a key (Ctrl B twice sends ^B)." : "Pane prefix off: Ctrl B goes to the terminal.", { timeout: 3500 });
    }, "gear");
    const beside = companionBeside();
    cmd(`Companion shell: ${beside ? "in place" : "beside the agent"}`, `${MOD} \` ${beside ? "now opens it beside (wide screens)" : "now flips in place"}`, () => {
      store.set("companion", beside ? "swap" : "split");
    }, "gear");
    launcher.paletteItems(cmd);
    search.paletteItems(cmd);
    commands.paletteItems(cmd);
    flywheel.paletteItems(cmd);
    review.paletteItems(cmd);
    mobile.paletteItems(cmd);
    notify.paletteItems(cmd);
    for (const workspace of state.workspaces) {
      cmd(`Edit workspace ${workspace.name}`, "rename, folders", () => openWorkspaceDialog(workspace), "gear");
    }
    for (const t of THEMES) {
      cmd(`Theme: ${t}`, t === theme ? "current" : "", () => applyTheme(t), "gear");
    }
    cmd("Keyboard shortcuts", "", () => $("#dlg-keys").showModal(), "chev");
    cmd("Sign out", "", logout, "x");
    return items;
  }

  // Subsequence match; returns [score, matched indices] or null.
  function fuzzy(query, text) {
    if (!query) return [0, []];
    const q = query.toLowerCase();
    const t = text.toLowerCase();
    const hits = [];
    let score = 0;
    let from = 0;
    let last = -2;
    for (const ch of q) {
      if (ch === " ") continue;
      const at = t.indexOf(ch, from);
      if (at < 0) return null;
      hits.push(at);
      score += at === last + 1 ? 6 : 1;
      if (at === 0 || /[\s/._-]/.test(t[at - 1])) score += 4;
      last = at;
      from = at + 1;
    }
    return [score - t.length * 0.02, hits];
  }

  function highlight(text, hits) {
    if (!hits.length) return [text];
    const set = new Set(hits);
    const out = [];
    let run = "";
    let marked = false;
    for (let i = 0; i < text.length; i++) {
      const m = set.has(i);
      if (m !== marked && run) {
        out.push(marked ? el("mark", { textContent: run }) : run);
        run = "";
      }
      marked = m;
      run += text[i];
    }
    if (run) out.push(marked ? el("mark", { textContent: run }) : run);
    return out;
  }

  function renderPalette() {
    const query = $("#palette-input").value.trim();
    const scored = [];
    for (const item of palette.items) {
      const match = fuzzy(query, item.label);
      const hintMatch = match ? null : fuzzy(query, item.hint);
      if (match) scored.push({ item, score: match[0] + 1, hits: match[1] });
      else if (hintMatch) scored.push({ item, score: hintMatch[0] - 5, hits: [] });
    }
    if (query) scored.sort((a, b) => b.score - a.score);
    palette.shown = scored;
    palette.index = Math.min(palette.index, Math.max(0, scored.length - 1));
    const list = $("#palette-list");
    const nodes = [];
    let group = null;
    scored.forEach(({ item, hits }, i) => {
      if (!query && item.group !== group) {
        group = item.group;
        nodes.push(el("li", { className: "palette-group", role: "presentation", textContent: group }));
      }
      nodes.push(
        el(
          "li",
          {
            className: `palette-item${item.warn ? " warn" : ""}`,
            role: "option",
            id: `palette-${i}`,
            "aria-selected": String(i === palette.index),
            onmousemove: () => {
              if (palette.index !== i) {
                palette.index = i;
                markPalette();
              }
            },
            onclick: () => runPalette(i),
          },
          el("span", { className: `glyph ${item.glyph}` }, icon(item.glyph)),
          el("span", { className: "label" }, highlight(item.label, hits)),
          el("span", { className: "hint", textContent: item.hint }),
        ),
      );
    });
    if (!scored.length) nodes.push(el("li", { className: "palette-none", textContent: "No match." }));
    list.replaceChildren(...nodes);
    markPalette();
  }

  function markPalette() {
    $("#palette-list").querySelectorAll(".palette-item").forEach((node) => {
      const selected = node.id === `palette-${palette.index}`;
      node.setAttribute("aria-selected", String(selected));
      if (selected) node.scrollIntoView({ block: "nearest" });
    });
    $("#palette-input").setAttribute("aria-activedescendant", `palette-${palette.index}`);
  }

  const paletteOpen = () => !$("#palette").hidden;

  // `query`: a filter to start from. `items`: a list of its own (the
  // quick-launch list) instead of every command, with `placeholder`.
  function openPalette(query, items, placeholder) {
    palette.items = items || paletteCommands();
    palette.index = 0;
    $("#palette-input").placeholder = placeholder || "Jump to a session or run a command";
    $("#palette-input").value = typeof query === "string" ? query : "";
    $("#palette").hidden = false;
    renderPalette();
    $("#palette-input").focus();
  }

  function closePalette() {
    $("#palette").hidden = true;
    focusTerminal();
  }

  function runPalette(i) {
    const entry = palette.shown[i];
    closePalette();
    if (entry) entry.item.run();
  }

  $("#palette-input").addEventListener("input", () => {
    palette.index = 0;
    renderPalette();
  });
  $("#palette-input").addEventListener("keydown", (event) => {
    const n = palette.shown.length;
    if (event.key === "ArrowDown" || (event.ctrlKey && event.key === "n")) {
      event.preventDefault();
      palette.index = n ? (palette.index + 1) % n : 0;
      markPalette();
    } else if (event.key === "ArrowUp" || (event.ctrlKey && event.key === "p")) {
      event.preventDefault();
      palette.index = n ? (palette.index - 1 + n) % n : 0;
      markPalette();
    } else if (event.key === "Enter") {
      event.preventDefault();
      runPalette(palette.index);
    } else if (event.key === "Escape") {
      event.preventDefault();
      closePalette();
    }
  });
  $("#palette").addEventListener("mousedown", (event) => {
    if (event.target === $("#palette")) closePalette();
  });

  // --- keyboard --------------------------------------------------------

  function jumpBy(delta) {
    const live = attachable();
    if (!live.length) return;
    const at = live.findIndex((s) => s.id === state.attached);
    const next = at < 0 ? (delta > 0 ? 0 : live.length - 1) : (at + delta + live.length) % live.length;
    openSession(live[next].id);
  }

  // The hub chord: Alt (no Ctrl/Meta) elsewhere, Ctrl+Option on macOS.
  function hubChord(event) {
    if (event.metaKey) return false;
    return IS_MAC ? event.ctrlKey && event.altKey : event.altKey && !event.ctrlKey;
  }

  // Whether the key reports its plain character (Alt+7 -> "7"), as
  // opposed to a layout character or a dead key.
  const typesItself = (event, ch) => event.key.length === 1 && event.key.toUpperCase() === ch;

  const ARROWS = { ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down" };
  const VIM = { KeyH: "left", KeyJ: "down", KeyK: "up", KeyL: "right" };

  // The tmux-style prefix (off unless turned on: palette "Pane prefix"):
  // Ctrl+B, then one key. Ctrl+B twice sends a literal ^B.
  const prefix = { on: store.get("prefix", false) === true, armed: false, timer: 0 };
  function armPrefix(armed) {
    prefix.armed = armed;
    clearTimeout(prefix.timer);
    if (armed) prefix.timer = setTimeout(() => armPrefix(false), 2500);
    $("#prefix-pill").hidden = !armed;
  }

  // Returns true when the key was the prefix's (and is consumed).
  function prefixKey(event) {
    if (!prefix.on || phone()) return false;
    const ctrlB = event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey && event.code === "KeyB";
    if (!prefix.armed) {
      if (!ctrlB) return false;
      event.preventDefault();
      event.stopPropagation();
      armPrefix(true);
      return true;
    }
    if (["Shift", "Control", "Alt", "Meta", "CapsLock"].includes(event.key)) return false;
    event.preventDefault();
    event.stopPropagation();
    armPrefix(false);
    const key = event.key;
    const pane = focusedPane();
    if (ctrlB) {
      if (pane) pane.send(JSON.stringify({ type: "input", data: "\x02" }));
    } else if (key === "%") {
      splitWithShell(pane, "right");
    } else if (key === '"') {
      splitWithShell(pane, "down");
    } else if (ARROWS[event.code] && event.ctrlKey) {
      nudgePane(ARROWS[event.code]);
    } else if (ARROWS[event.code]) {
      focusToward(ARROWS[event.code]);
    } else if (key === "z") {
      toggleZoom();
    } else if (key === "x") {
      closePane();
    } else if (key === "c") {
      newTab();
    } else if (key === "n") {
      cycleTab(1);
    } else if (key === "p") {
      cycleTab(-1);
    } else if (key === "o") {
      cyclePane(1);
    } else if (/^[1-9]$/.test(key)) {
      const layout = viewLayout();
      const t = layout && layout.tabs[Number(key) - 1];
      if (t) selectTab(t.id);
    } else if (key === ",") {
      const t = activeTab();
      if (t) {
        tabRenaming = t.id;
        renderTabs();
      }
    } else if (key === "&") {
      const t = activeTab();
      if (t) closeTab(t.id);
    } else if (key === "?") {
      $("#dlg-keys").showModal();
    }
    return true;
  }

  // Capture phase: runs before xterm sees the key, so hub shortcuts never
  // reach the shell (Ctrl+K twice sends a literal ^K).
  window.addEventListener(
    "keydown",
    (event) => {
      const mod = event.ctrlKey || event.metaKey;
      if (mod && !event.altKey && !event.shiftKey && event.code === "KeyK") {
        event.preventDefault();
        event.stopPropagation();
        if (paletteOpen()) {
          closePalette();
          if (focusedPane()) focusedPane().send(JSON.stringify({ type: "input", data: "\x0b" }));
        } else if (!document.querySelector("dialog[open]")) {
          search.close(false);
          openPalette();
        }
        return;
      }
      // Ctrl Shift F (Cmd Shift F on macOS): search every session.
      if (mod && event.shiftKey && !event.altKey && event.code === "KeyF") {
        event.preventDefault();
        event.stopPropagation();
        if (paletteOpen()) closePalette();
        if (!document.querySelector("dialog[open]")) search.toggle();
        return;
      }
      if (paletteOpen() || search.isOpen() || document.querySelector("dialog[open]") || $("#ctx-menu")) return;
      if (prefixKey(event)) return;
      if (!hubChord(event)) return;
      let handled = true;
      const digit = /^Digit([1-9])$/.exec(event.code);
      const letter = /^Key([A-Z])$/.exec(event.code);
      if ((digit || letter) && !IS_MAC && !typesItself(event, (digit || letter)[1])) {
        // The layout made Alt+key into a character of its own: the
        // terminal's, not a hub shortcut.
        handled = false;
      } else if (digit) {
        const target = attachable()[Number(digit[1]) - 1];
        if (target) openSession(target.id);
        else handled = false;
      } else if (event.code === "KeyN") {
        if (event.shiftKey) openWorkspaceDialog();
        else openSessionDialog();
      } else if (event.code === "ArrowDown" && !event.shiftKey) {
        jumpBy(1);
      } else if (event.code === "ArrowUp" && !event.shiftKey) {
        jumpBy(-1);
      } else if (event.code === "KeyR" && !event.shiftKey && state.attached) {
        startRename(state.attached);
      } else if (event.code === "KeyW" && !event.shiftKey && state.attached) {
        closeSession(state.attached);
      } else if (event.code === "KeyC") {
        if (event.shiftKey) commands.copyLast();
        else {
          review.close();
          commands.toggle();
        }
      } else if (event.code === "Backquote" && !event.shiftKey && state.attached) {
        commands.flipCompanion();
      } else if (event.code === "KeyF" && !event.shiftKey) {
        flywheel.togglePill();
      } else if (event.code === "KeyG" && !event.shiftKey) {
        review.togglePill();
      } else if (event.code === "Backslash" && !event.shiftKey && (IS_MAC || event.key === "\\")) {
        splitWithShell(focusedPane(), "right");
      } else if (event.code === "Minus" && !event.shiftKey && (IS_MAC || event.key === "-")) {
        splitWithShell(focusedPane(), "down");
      } else if (event.shiftKey && ARROWS[event.code]) {
        focusToward(ARROWS[event.code]);
      } else if (event.shiftKey && VIM[event.code]) {
        nudgePane(VIM[event.code]);
      } else if (event.code === "KeyZ" && !event.shiftKey) {
        toggleZoom();
      } else if (event.code === "KeyX" && !event.shiftKey && state.focusId && !phone()) {
        closePane();
      } else if (event.code === "KeyT" && !event.shiftKey && state.view && !phone()) {
        newTab();
      } else if (event.code === "PageDown" || event.code === "PageUp") {
        cycleTab(event.code === "PageDown" ? 1 : -1);
      } else {
        handled = false;
      }
      if (handled) {
        event.preventDefault();
        event.stopPropagation();
      }
    },
    true,
  );

  // --- chrome actions --------------------------------------------------

  async function logout() {
    pwa.forget();
    mobile.forget();
    await fetch("/logout", { method: "POST", credentials: "same-origin" });
    window.location.replace("/");
  }

  const actions = {
    "new-session": () => openSessionDialog(),
    "new-workspace": () => openWorkspaceDialog(),
    palette: () => openPalette(),
    "from-template": () => {
      document.body.classList.remove("side-open");
      openPalette("", launcher.templateItems(), "Launch from a template");
    },
    "cycle-theme": () => applyTheme(THEMES[(THEMES.indexOf(theme) + 1) % THEMES.length]),
    logout,
    "toggle-sidebar": () => document.body.classList.toggle("side-open"),
    "toggle-commands": () => {
      review.close();
      commands.toggle();
    },
    "flip-companion": () => commands.flipCompanion(),
    "toggle-flywheel": () => flywheel.togglePill(),
    "toggle-review": () => review.togglePill(),
    search: () => search.open(),
    machines: () => {
      document.body.classList.remove("side-open");
      machines.openDialog();
    },
  };
  document.addEventListener("click", (event) => {
    const target = event.target.closest("[data-action]");
    if (target && actions[target.dataset.action]) actions[target.dataset.action]();
  });

  // --- boot ------------------------------------------------------------

  // No #session (a fresh tab, a restart): bring back the last workspace
  // this browser had open -- else the first with a saved layout -- at
  // its active tab's focused pane.
  async function restoreWorkspace() {
    const remembered = workspaceById(state.currentWorkspace);
    const order = [remembered, ...state.workspaces.filter((w) => w !== remembered)].filter(Boolean);
    for (const workspace of order) {
      let layout;
      try {
        layout = await ensureLayout(workspace.id);
      } catch (_) {
        continue;
      }
      if (state.attached || window.location.hash) return;
      if (!layout || !layout.tabs.length) continue;
      const view = viewOf(workspace.id);
      const tab = layout.tabs.find((t) => t.id === view.tab) || layout.tabs[0];
      const leaves = L.panes(tab.root).filter((leaf) => leaf.session && sessionById(leaf.session));
      const leaf = leaves.find((p) => p.id === view.focus[tab.id]) || leaves[0];
      const session = leaf ? leaf.session
        : layout.tabs.flatMap((t) => L.sessionsOf(t.root)).find((id) => sessionById(id));
      if (session) {
        await openSession(session);
        return;
      }
    }
  }

  async function boot() {
    // Measure terminal cells with the real font, not a fallback.
    const fonts = document.fonts
      ? Promise.all([
        document.fonts.load('13px "FW Term"'),
        document.fonts.load('600 13px "FW Term"'),
      ])
      : Promise.resolve();
    await Promise.race([fonts, new Promise((r) => setTimeout(r, 1500))]).catch(() => {});
    applyTheme(theme);
    loadKinds();
    // Static chord labels follow the platform's hub modifier.
    document.querySelectorAll("kbd").forEach((k) => {
      if (k.textContent === "Alt") k.textContent = MOD;
    });
    document.querySelectorAll("[title]").forEach((node) => {
      node.title = node.title.replace(/\bAlt\b/g, MOD);
    });
    // Other machines' lists first, so a #session= of theirs re-attaches.
    await machines.start();
    try {
      await refresh();
    } catch (error) {
      showError(error);
    }
    // Re-attach the session named in the URL (survives reloads).
    const match = /^#session=([0-9a-f]+)$/.exec(window.location.hash);
    if (match) {
      if (sessionById(match[1])) openSession(match[1]);
      else detach(STALE_LINK, "Session removed.");
    } else if (!phone()) {
      // A phone lands on its start screen (Needs you): opening a layout
      // pane here would mark an unseen finished turn seen unasked.
      await restoreWorkspace().catch(() => {});
    }
    // Follow an edited or navigated #session= (replaceState fires no
    // hashchange, so this sees only the user's own changes).
    window.addEventListener("hashchange", () => {
      const target = /^#session=([0-9a-f]+)$/.exec(window.location.hash);
      if (!target) {
        if (state.attached) detach();
        return;
      }
      if (target[1] === state.attached) return;
      if (sessionById(target[1])) openSession(target[1]);
      else if (state.attached) history.replaceState(null, "", `#session=${state.attached}`);
      else detach(STALE_LINK, "Session removed.");
    });
    startPolling();
    connectEvents();
  }

  boot();
})();

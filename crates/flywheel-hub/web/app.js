// flywheel hub web client: workspaces > sessions in the sidebar, one
// attached terminal, dialogs, a command palette, and keyboard shortcuts.
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
    attached: null, // session id
    socket: null,
    exited: null, // exit info of the attached session's process
    replaying: false, // a snapshot is being replayed at the session's size
    retry: 0, // reconnect attempts since the last successful attach
    retryTimer: null,
    countdown: null,
    currentWorkspace: store.get("workspace", null), // last workspace touched
    collapsed: new Set(store.get("collapsed", [])),
    repoChoice: store.get("repo-choice", {}), // workspace id -> repo
    wsErrors: {}, // workspace id -> last action error
    renaming: null, // session id being renamed inline
    snapshot: "",
    offline: 0, // consecutive failed refreshes
    notice: "", // why the last session detached, shown on the empty stage
    noticeTitle: "", // headline for that notice (e.g. "Session removed")
    attachedMeta: null, // {name, workspaceId, workspaceName} of the attached session, kept to explain its removal
    closing: new Set(), // session ids this browser is removing (no "removed elsewhere" notice)
    refreshSeq: 0, // refreshes started (each response is tagged with its number)
    refreshApplied: 0, // the newest refresh whose response was applied
    attachedSeq: 0, // refreshSeq when the current session was attached
    dropped: 0, // keystrokes typed while the link was down
    tick: null, // repaints the reconnect banner
    painted: false, // first sidebar paint done (entry animation plays once)
    agent: {}, // session id -> {status, detail, since}, as this client sees it
    rollup: {}, // workspace id -> most urgent agent status
    events: null, // the hub-wide events socket
    eventsRetry: 0,
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
      return session.code === 0
        ? { dot: "done", label: "done", tone: "" }
        : { dot: "bad", label: `exit ${session.code}`, tone: "bad" };
    }
    if (session.restore_error) return { dot: "bad", label: "not restored", tone: "bad" };
    return { dot: "stopped", label: "stopped", tone: "" };
  }

  const exitHow = (info) => (info.signal ? `killed by ${info.signal}` : `exit code ${info.code}`);

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

  async function api(method, path, body) {
    const init = { method, credentials: "same-origin", headers: {} };
    if (body !== undefined) {
      init.headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    const response = await fetch(path, init);
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
    if (!response.ok) throw new Error((data && data.error) || response.statusText);
    return data;
  }

  // The Commands drawer (commands.js): agents' suggested code blocks.
  const commands = window.FwHubCommands({
    $,
    el,
    icon,
    api,
    state,
    sessionById,
    workspaceById,
    MOD,
    openSession: (id) => openSession(id),
    refresh: () => refresh(),
    termSize: () => ({ cols: term.cols, rows: term.rows }),
    focusTerminal: () => {
      if (state.attached && term && !document.querySelector("dialog[open]")) term.focus();
    },
  });

  // The phone and alerts modules (pwa.js, attention.js, notify.js,
  // mobile.js).
  const pwa = window.FwHubPwa();
  const attention = window.FwHubAttention({
    $, el, icon, state, workspaceById, agentStatus, openSession: (id) => openSession(id),
  });
  const notify = window.FwHubNotify({
    $, state, store, sessionById, workspaceById, pwa, api, attention, toast: commands.toast,
    openSession: (id) => openSession(id),
    present: () => present(),
  });
  // Sibling worktrees (worktree.js): the new-session fields, row badges,
  // and the keep / remove close dialog.
  const worktrees = window.FwHubWorktree({ $, el, icon, api });

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
      [ws, ss] = await Promise.all([api("GET", "/api/workspaces"), api("GET", "/api/sessions")]);
    } catch (error) {
      if (seq < state.refreshApplied) return;
      state.offline += 1;
      $("#offline").hidden = state.offline < 2;
      throw error;
    }
    if (seq < state.refreshApplied) return;
    state.refreshApplied = seq;
    const current = seq > state.attachedSeq;
    state.offline = 0;
    $("#offline").hidden = true;
    state.workspaces = ws.workspaces;
    state.sessions = ss.sessions;
    mobile.pruneDrafts(state.sessions.map((s) => s.id));
    // The attached session was removed (closed in another browser or by
    // the CLI, or its workspace was deleted): there is nothing to relaunch
    // or reconnect to, so detach and say what happened.
    if (state.attached && current && !sessionById(state.attached)) {
      const gone = state.attachedMeta;
      const ours = state.closing.has(state.attached);
      state.snapshot = JSON.stringify([ws, ss]);
      if (ours) {
        detach();
      } else if (gone && !workspaceById(gone.workspaceId)) {
        detach(`Workspace ${gone.workspaceName} was deleted, and ${gone.name} with it.`, "Session removed.");
      } else {
        detach(`${gone ? gone.name : "That session"} was closed elsewhere (another browser or fw hub).`, "Session removed.");
      }
      return;
    }
    rememberAttached();
    // A lost link whose session is gone will never come back: stop waiting.
    const attached = sessionById(state.attached);
    if (state.attached && current && !state.socket && !state.exited && (!attached || !attached.live)) {
      state.snapshot = JSON.stringify([ws, ss]);
      if (attached) showGone(attached.id, true);
      else detach("That session was closed.");
      return;
    }
    const snapshot = JSON.stringify([ws, ss]);
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
    if (term) {
      term.options.theme = TERM_THEMES[resolvedTheme()];
      term.options.minimumContrastRatio = contrastFloor();
    }
    sendTheme();
  }
  // The server answers programs' colour queries (OSC 4/10/11/12) for every
  // viewer, so it is told this one's palette on attach, theme change, and
  // focus: the last-focused viewer's colours win.
  const ANSI_KEYS = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue", "brightMagenta",
    "brightCyan", "brightWhite"];
  function sendTheme() {
    const colors = TERM_THEMES[resolvedTheme()];
    send(JSON.stringify({
      type: "theme",
      foreground: colors.foreground,
      background: colors.background,
      cursor: colors.cursor,
      ansi: ANSI_KEYS.map((key) => colors[key]),
    }));
  }
  window.addEventListener("focus", () => sendTheme());
  // Agent TUIs (Claude Code) paint with 256-colour indexes tuned for dark
  // backgrounds -- pale blue, gold -- that vanish on the light theme's
  // paper. xterm lifts any cell below this ratio against its background.
  const contrastFloor = () => (resolvedTheme() === "light" ? 4.5 : 1);
  prefersLight.addEventListener("change", () => applyTheme(theme));

  // --- terminal --------------------------------------------------------

  const TERM_FONT = '"FW Term", ui-monospace, "SF Mono", Menlo, Consolas, monospace';
  let term = null;
  let fit = null;
  const mobile = window.FwHubMobile({
    $, el, state, store, api, sessionById, agentStatus,
    toast: commands.toast,
    copyText: commands.copyText,
    term: () => term,
    fit: () => fit,
    sendData: (data) => sendInput(JSON.stringify({ type: "input", data })),
    onKey: () => term && term.scrollToBottom(),
  });

  function createTerminal() {
    term = new window.Terminal({
      cursorBlink: true,
      fontFamily: TERM_FONT,
      fontSize: mobile.fontSize(),
      lineHeight: 1.12,
      scrollback: 10000,
      allowProposedApi: false,
      theme: TERM_THEMES[resolvedTheme()],
      minimumContrastRatio: contrastFloor(),
    });
    fit = new window.FitAddon.FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new window.WebLinksAddon.WebLinksAddon());
    term.open($("#terminal"));
    // The server's emulator answers terminal queries for every session, so
    // the browser must not: a second viewer would answer twice.
    const answered = () => true;
    for (const id of [{ final: "c" }, { prefix: ">", final: "c" }, { final: "n" },
      { prefix: "?", final: "n" }, { intermediates: "$", final: "p" },
      { prefix: "?", intermediates: "$", final: "p" }]) {
      term.parser.registerCsiHandler(id, answered);
    }
    term.parser.registerDcsHandler({ intermediates: "$", final: "q" }, answered);
    // OSC 4 is "index;spec" pairs, OSC 10-12 a spec list; "?" asks.
    const onlyQueries = (specs) => specs.length > 0 && specs.every((spec) => spec === "?");
    term.parser.registerOscHandler(4, (data) => onlyQueries(data.split(";").filter((_, i) => i % 2 === 1)));
    for (const osc of [10, 11, 12]) {
      term.parser.registerOscHandler(osc, (data) => onlyQueries(data.split(";")));
    }
    // Sticky Ctrl/Alt from the phone's key bar apply to what is typed.
    term.onData((data) => sendInput(JSON.stringify({ type: "input", data: mobile.transformInput(data) })));
    term.onBinary((data) => {
      const bytes = new Uint8Array(data.length);
      for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i) & 0xff;
      sendInput(bytes);
    });
    term.onResize(({ cols, rows }) => {
      send(JSON.stringify({ type: "resize", cols, rows }));
      updateMeters();
    });
    let pending = 0;
    // While a snapshot replays the terminal keeps the size it was drawn at.
    const refit = () => {
      cancelAnimationFrame(pending);
      pending = requestAnimationFrame(() => {
        if (!state.replaying && $("#terminal").clientWidth > 0) mobile.fitTerminal();
      });
    };
    window.addEventListener("resize", refit);
    new ResizeObserver(refit).observe($("#terminal"));
    mobile.fitTerminal();
    mobile.attachTerminal();
  }

  function send(message) {
    const socket = state.socket;
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(message);
      return true;
    }
    return false;
  }

  // Keystrokes cannot be queued safely (a reconnect replays a screen they
  // were not typed against), so they are dropped -- and the banner says so.
  function sendInput(message) {
    if (send(message) || !state.attached || state.exited) return;
    state.dropped += 1;
    if (state.retryTimer) retryTick();
  }

  // --- connection ------------------------------------------------------

  function setLink(kind, text) {
    const pill = $("#link");
    pill.dataset.link = kind;
    pill.textContent = text || kind;
  }

  function showBanner(text, actions, calm) {
    $("#banner").hidden = false;
    $("#banner").classList.toggle("calm", !!calm);
    $("#banner-text").textContent = text;
    $("#banner-actions").replaceChildren(...(actions || []).filter(Boolean));
  }

  function hideBanner() {
    $("#banner").hidden = true;
  }

  const retryTick = () => {
    if (state.tick) state.tick();
  };

  function stopRetry() {
    clearTimeout(state.retryTimer);
    clearInterval(state.countdown);
    state.retryTimer = null;
    state.countdown = null;
    state.tick = null;
  }

  function closeSocket() {
    stopRetry();
    if (state.socket) {
      state.socket.onclose = null;
      state.socket.close(1000);
      state.socket = null;
    }
    endReplay();
  }

  // Stop holding the terminal at a snapshot's size (the replay finished or
  // its socket went away) and fit it back to this viewport.
  function endReplay() {
    if (!state.replaying) return;
    state.replaying = false;
    if ($("#terminal").clientWidth > 0) mobile.fitTerminal();
  }

  function detach(message, title) {
    closeSocket();
    state.attached = null;
    state.attachedMeta = null;
    state.exited = null;
    term.reset();
    history.replaceState(null, "", window.location.pathname);
    hideBanner();
    setLink("idle");
    state.notice = message || "";
    state.noticeTitle = (message && title) || "";
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

  function attach(id) {
    const session = sessionById(id);
    if (!session || !session.live) return;
    closeSocket();
    state.attached = id;
    state.attachedSeq = state.refreshSeq;
    state.exited = null;
    state.retry = 0;
    state.dropped = 0;
    state.notice = "";
    state.noticeTitle = "";
    rememberAttached();
    delete state.wsErrors[session.workspace_id];
    rememberWorkspace(session.workspace_id);
    history.replaceState(null, "", `#session=${id}`);
    hideBanner();
    document.body.classList.remove("side-open");
    term.reset();
    connect(id);
    render();
    sendView();
  }

  // Show a session that has no terminal on this hub (the hub restarted):
  // the last screen stays when `keepScreen` (it was attached when the
  // link died), with Relaunch and Close.
  function showGone(id, keepScreen) {
    const session = sessionById(id);
    if (!session) return;
    closeSocket();
    state.attached = id;
    state.attachedSeq = state.refreshSeq;
    state.exited = { gone: true };
    state.notice = "";
    state.noticeTitle = "";
    rememberAttached();
    delete state.wsErrors[session.workspace_id];
    rememberWorkspace(session.workspace_id);
    history.replaceState(null, "", `#session=${id}`);
    document.body.classList.remove("side-open");
    if (!keepScreen) term.reset();
    term.write(`\r\n\x1b[2m[no terminal on this hub: ${status(session).label}]\x1b[0m\r\n`);
    setLink("exited", "gone");
    showBanner(goneText(session), endedActions(id), true);
    render();
    sendView();
  }

  function endedActions(id) {
    return [
      el("button", { type: "button", className: "slab", onclick: () => relaunch(id) }, "Relaunch"),
      el("button", { type: "button", className: "slab ghost", onclick: () => closeSession(id) }, "Close"),
    ];
  }

  // Attach a session with a terminal; show an ended one's state.
  function openSession(id) {
    const session = sessionById(id);
    if (!session) return;
    if (session.live) attach(id);
    else showGone(id, false);
  }

  // Open (or re-open) the attached session's socket. Each connect replays
  // a snapshot of the session's terminal, so the terminal is reset first.
  function connect(id) {
    stopRetry();
    setLink("connecting", state.retry ? `retry ${state.retry}` : "connecting");
    const scheme = window.location.protocol === "https:" ? "wss" : "ws";
    const socket = new WebSocket(`${scheme}://${window.location.host}/api/sessions/${id}/ws`);
    socket.binaryType = "arraybuffer";
    state.socket = socket;
    state.replaying = false;
    socket.onopen = () => {
      // The snapshot follows; clear only now, so a dead link keeps the last
      // screen visible while it retries.
      term.reset();
      state.retry = 0;
      state.dropped = 0;
      setLink("attached");
      sendTheme();
      hideBanner();
      term.focus();
      refresh(false).catch(() => {});
    };
    // The snapshot is drawn at the session's size: replay exactly its
    // bytes at that size, then fit to this viewport and resize the session
    // (the program redraws).
    let pending = 0;
    const replayed = () => {
      if (state.socket !== socket) return;
      state.replaying = false;
      if ($("#terminal").clientWidth > 0) mobile.fitTerminal();
      send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
    };
    socket.onmessage = (event) => {
      if (typeof event.data !== "string") {
        const bytes = new Uint8Array(event.data);
        if (pending > 0) {
          pending -= bytes.length;
          term.write(bytes, pending <= 0 ? replayed : undefined);
        } else {
          term.write(bytes);
        }
        return;
      }
      let message;
      try {
        message = JSON.parse(event.data);
      } catch (_) {
        return;
      }
      if (message.type === "snapshot") {
        state.replaying = true;
        term.resize(message.cols, message.rows);
        pending = message.bytes;
        if (!(pending > 0)) replayed();
      } else if (message.type === "reset") {
        term.reset();
      } else if (message.type === "exit") {
        onExit(id, message);
      } else if (message.type === "error") {
        showBanner(message.message, [], true);
      }
    };
    socket.onclose = (event) => {
      if (state.socket !== socket) return;
      state.socket = null;
      endReplay();
      // 4001: the hub's token was rotated; this login no longer counts.
      if (event.code === 4001) {
        window.location.replace("/?signed_out=rotated" + window.location.hash);
        return;
      }
      if (state.exited || state.attached !== id) return;
      scheduleReconnect(id, event.code);
    };
  }

  function onExit(id, message) {
    const how = exitHow(message);
    state.exited = message;
    term.write(`\r\n\x1b[2m[process ended: ${how}]\x1b[0m\r\n`);
    setLink("exited", "ended");
    const session = sessionById(id);
    showBanner(
      `Process ended (${how}). Scrollback stays until the session is closed or relaunched.`,
      endedActions(id),
      !message.signal && message.code === 0,
    );
    if (session) refresh().catch(showError);
  }

  function scheduleReconnect(id, code) {
    state.retry += 1;
    const delay = Math.min(10000, 500 * 2 ** Math.min(state.retry - 1, 5));
    const due = Date.now() + delay;
    setLink("lost", "link lost");
    const now = el("button", { type: "button", className: "slab", onclick: () => reconnectNow(id) }, "Reconnect now");
    const tick = () => {
      const seconds = Math.max(0, Math.ceil((due - Date.now()) / 1000));
      const why = state.offline
        ? "The hub is unreachable."
        : `Link lost (code ${code}); the session keeps running on the hub.`;
      const dropped = state.dropped
        ? ` ${state.dropped} keystroke${state.dropped === 1 ? "" : "s"} not sent.`
        : "";
      $("#banner-text").textContent = `${why} Reconnecting in ${seconds}s.${dropped}`;
    };
    state.tick = tick;
    showBanner("", [now]);
    tick();
    state.countdown = setInterval(tick, 250);
    state.retryTimer = setTimeout(() => reconnectNow(id), delay);
  }

  async function reconnectNow(id) {
    stopRetry();
    if (state.attached !== id || state.socket) return;
    try {
      await refresh();
    } catch (_) {
      // Hub unreachable: try the socket anyway; its failure reschedules.
    }
    // The refresh may have settled it already (session gone or closed).
    if (state.attached !== id || state.exited || state.socket) return;
    const session = sessionById(id);
    if (state.offline === 0 && !session) {
      detach("That session was closed.", "Session removed.");
      return;
    }
    if (state.offline === 0 && !session.live) {
      showGone(id, true);
      return;
    }
    connect(id);
  }

  const wakeUp = () => {
    if (state.attached && !state.socket && !state.exited) reconnectNow(state.attached);
  };
  window.addEventListener("online", wakeUp);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") wakeUp();
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
  function currentView() {
    const session = sessionById(state.attached);
    return present() && session && session.live ? session.id : null;
  }

  function sendView(force) {
    const socket = state.events;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    const view = currentView();
    if (!force && view === state.viewing) return;
    state.viewing = view;
    socket.send(JSON.stringify({ type: "view", session_id: view }));
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
    };
    socket.onmessage = (event) => {
      let message;
      try {
        message = JSON.parse(event.data);
      } catch (_) {
        return;
      }
      if (message.type === "snapshot") {
        state.agent = message.sessions || {};
        state.rollup = message.workspaces || {};
        renderStatus();
      } else if (message.type === "status") {
        notify.onStatus(message.session_id, state.agent[message.session_id], message.status);
        if (message.status) state.agent[message.session_id] = message.status;
        else delete state.agent[message.session_id];
        if (message.workspace_id) {
          if (message.workspace_status) state.rollup[message.workspace_id] = message.workspace_status;
          else delete state.rollup[message.workspace_id];
        }
        renderStatus();
      } else if (message.type === "sessions") {
        refresh(false).catch(() => {});
      } else if (message.type === "commands") {
        commands.onEvent(message);
      }
    };
    socket.onclose = (event) => {
      if (state.events !== socket) return;
      state.events = null;
      state.viewing = undefined;
      // 4001: the hub's token was rotated (as on the terminal socket).
      if (event.code === 4001) {
        window.location.replace("/?signed_out=rotated" + window.location.hash);
        return;
      }
      startPolling();
      state.eventsRetry += 1;
      const delay = Math.min(10000, 500 * 2 ** Math.min(state.eventsRetry - 1, 5));
      state.eventsTimer = setTimeout(connectEvents, delay);
    };
  }

  const eventsWake = () => {
    if (!state.events) connectEvents();
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

  async function createSession(body) {
    const created = await api("POST", "/api/sessions", {
      ...body,
      cols: term.cols,
      rows: term.rows,
    });
    await refresh();
    attach(created.id);
    return created;
  }

  // Relaunch in place: same record (name, place in the sidebar), a fresh
  // process and scrollback.
  async function relaunch(id) {
    if (!sessionById(id)) return;
    try {
      await api("POST", `/api/sessions/${id}/restart`, { cols: term.cols, rows: term.rows });
      await refresh();
      attach(id);
    } catch (error) {
      showBanner(`Relaunch failed: ${error.message}`, endedActions(id), false);
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
      if (state.attached === id || closing.includes(state.attached)) detach();
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
    if (state.attached && !document.querySelector("dialog[open]") && !paletteOpen()) term.focus();
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
        if (state.attached === session.id) detach();
      }
      await api("DELETE", `/api/workspaces/${workspace.id}`);
      if (sessions.some((s) => s.id === state.attached)) detach();
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
        className: `session${active ? " active" : ""}${session.live ? "" : " gone"}${st.dot === "blocked" ? " needs-you" : ""}${session.companion_of ? " companion" : ""}`,
        dataset: { id: session.id },
        tabIndex: 0,
        role: "button",
        "aria-current": active ? "true" : undefined,
        title: session.live ? undefined : goneText(session),
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
        el("span", { className: `session-state ${st.tone}`, textContent: st.label, title: st.title || undefined }),
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
            el("span", {
              className: f.missing ? "missing" : f.git ? "git" : "plain",
              textContent: folderLabel(f),
              title: f.missing
                ? `${f.path} (missing: moved or deleted?)`
                : f.git ? `${f.path} (git repo)` : `${f.path} (plain folder, no git)`,
            })),
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

  function render() {
    const numbering = new Map(attachable().map((s, i) => [s.id, i]));
    const root = $("#workspaces");
    if (state.workspaces.length === 0) {
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
    renderMain();
  }

  // The header's agent / companion-shell flip (Alt+`, which phones lack).
  function renderFlip(session) {
    const button = $("#flip-toggle");
    const parent = session && session.companion_of ? sessionById(session.companion_of) : null;
    const agent = session && (session.kind === "claude" || session.kind === "codex");
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
    const workspace = session ? workspaceById(session.workspace_id) : null;
    $("#empty").hidden = !!session;
    $("#crumb-workspace").textContent = workspace ? workspace.name : "";
    $("#crumb-session").textContent = session ? session.name : "No session";
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

    commands.render();
    commands.prefetch();
    mobile.render();

    const actions = $("#empty-actions");
    if (!session) {
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
    dialog.addEventListener("close", () => {
      if (state.attached && !document.querySelector("dialog[open]")) term.focus();
    });
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
    if (workspace) workspace.folders.forEach((f) => addRepoRow(f));
    else addRepoRow("");
    importer.reset(!workspace);
    $("#dlg-workspace").showModal();
    wsForm.name.focus();
  }

  $("#add-repo").addEventListener("click", () => addRepoRow("").input.focus());
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

  async function loadKinds() {
    try {
      state.kinds = (await api("GET", "/api/kinds")).kinds;
    } catch (_) {
      state.kinds = state.kinds || [{ kind: "shell", available: true, program: null }];
    }
    return state.kinds;
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
  }

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

  async function openSessionDialog(workspaceId) {
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
      ...state.workspaces.map((w) => el("option", { value: w.id, textContent: w.name, selected: w.id === target.id })),
    );
    renderKinds(state.kinds || [{ kind: "shell", available: true, program: null }], store.get("kind", "shell"));
    renderRepoPicker();
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

  $("#session-workspace").addEventListener("change", renderRepoPicker);
  $("#repo-picker").addEventListener("change", pickedFolderChanged);
  $("#repo-picker").addEventListener("input", (event) => {
    if (event.target.matches(".repo-filter")) pickedFolderChanged();
  });
  $("#wt-pick").addEventListener("input", updateSessionPlaceholder);
  // Enter launches from anywhere in the form (radios do not submit on
  // their own).
  sessionForm.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.isComposing && !event.target.matches("button, select")) {
      event.preventDefault();
      sessionForm.requestSubmit();
    }
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
      const body = { workspace_id: workspaceId, kind };
      if (repoInput) body.repo = repoInput.value;
      const worktree = worktrees.request();
      if (worktree) body.worktree = worktree;
      if (name) body.name = name;
      if (repoInput) {
        state.repoChoice[workspaceId] = repoInput.value;
        store.set("repo-choice", state.repoChoice);
      }
      store.set("kind", kind);
      $("#dlg-session").close("ok");
      await createSession(body);
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
        hint: `${workspace ? workspace.name : ""} · ${status(session).label}${n < 9 ? ` · ${MOD} ${n + 1}` : ""}${session.live ? "" : " · no terminal"}`,
        run: () => openSession(session.id),
      });
    }
    const cmd = (label, hint, run, glyph = "chev") => items.push({ group: "Commands", glyph, label, hint, run });
    cmd("New session", `${MOD} N`, () => openSessionDialog(), "plus");
    for (const workspace of state.workspaces) {
      cmd(`New session in ${workspace.name}`, "", () => openSessionDialog(workspace.id), "plus");
    }
    cmd("New workspace", `${MOD} Shift N`, () => openWorkspaceDialog(), "plus");
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
      cmd("Detach", "leave the session running", () => detach(), "chev");
    }
    commands.paletteItems(cmd);
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
            className: "palette-item",
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

  function openPalette() {
    palette.items = paletteCommands();
    palette.index = 0;
    $("#palette-input").value = "";
    $("#palette").hidden = false;
    renderPalette();
    $("#palette-input").focus();
  }

  function closePalette() {
    $("#palette").hidden = true;
    if (state.attached && !document.querySelector("dialog[open]")) term.focus();
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
    attach(live[next].id);
  }

  // The hub chord: Alt (no Ctrl/Meta) elsewhere, Ctrl+Option on macOS.
  function hubChord(event) {
    if (event.metaKey) return false;
    return IS_MAC ? event.ctrlKey && event.altKey : event.altKey && !event.ctrlKey;
  }

  // Whether the key reports its plain character (Alt+7 -> "7"), as
  // opposed to a layout character or a dead key.
  const typesItself = (event, ch) => event.key.length === 1 && event.key.toUpperCase() === ch;

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
          if (state.attached) send(JSON.stringify({ type: "input", data: "\x0b" }));
        } else if (!document.querySelector("dialog[open]")) {
          openPalette();
        }
        return;
      }
      if (paletteOpen() || document.querySelector("dialog[open]")) return;
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
        if (target) attach(target.id);
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
        else commands.toggle();
      } else if (event.code === "Backquote" && !event.shiftKey && state.attached) {
        commands.flipCompanion();
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
    palette: openPalette,
    "cycle-theme": () => applyTheme(THEMES[(THEMES.indexOf(theme) + 1) % THEMES.length]),
    logout,
    "toggle-sidebar": () => document.body.classList.toggle("side-open"),
    "toggle-commands": () => commands.toggle(),
    "flip-companion": () => commands.flipCompanion(),
  };
  document.addEventListener("click", (event) => {
    const target = event.target.closest("[data-action]");
    if (target && actions[target.dataset.action]) actions[target.dataset.action]();
  });

  // --- boot ------------------------------------------------------------

  async function boot() {
    // Measure terminal cells with the real font, not a fallback.
    const fonts = document.fonts
      ? Promise.all([
        document.fonts.load('13px "FW Term"'),
        document.fonts.load('600 13px "FW Term"'),
      ])
      : Promise.resolve();
    await Promise.race([fonts, new Promise((r) => setTimeout(r, 1500))]).catch(() => {});
    createTerminal();
    applyTheme(theme);
    loadKinds();
    // Static chord labels follow the platform's hub modifier.
    document.querySelectorAll("kbd").forEach((k) => {
      if (k.textContent === "Alt") k.textContent = MOD;
    });
    document.querySelectorAll("[title]").forEach((node) => {
      node.title = node.title.replace(/\bAlt\b/g, MOD);
    });
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

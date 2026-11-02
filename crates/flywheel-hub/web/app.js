// flywheel hub web client: workspaces > sessions in the sidebar, one
// attached terminal, dialogs, a command palette, and keyboard shortcuts.
// Plain JS, no build step. Talks to the hub's REST API and per-session
// WebSocket (protocol: crates/flywheel-hub/src/server.rs).
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

  const basename = (path) => path.replace(/\/+$/, "").split("/").pop() || path;
  const pad2 = (n) => String(n).padStart(2, "0");
  const KIND_GLYPH = { shell: "shell", claude: "claude", codex: "codex" };
  const glyphFor = (kind) => KIND_GLYPH[kind] || "shell";

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
    painted: false, // first sidebar paint done (entry animation plays once)
  };

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

  function status(session) {
    if (!session.live) return { dot: "gone", label: "gone", tone: "" };
    if (session.state === "running") return { dot: "live", label: "live", tone: "live" };
    if (session.state === "exited") {
      if (session.signal) return { dot: "bad", label: session.signal, tone: "bad" };
      return session.code === 0
        ? { dot: "done", label: "done", tone: "" }
        : { dot: "bad", label: `exit ${session.code}`, tone: "bad" };
    }
    return { dot: "stopped", label: "stopped", tone: "" };
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
      window.location.replace("/");
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

  // `force` re-renders even when nothing changed. The periodic refresh
  // passes false and also holds off while a sidebar field has focus, so a
  // half-typed rename is never torn down under the user.
  async function refresh(force = true) {
    let ws;
    let ss;
    try {
      [ws, ss] = await Promise.all([api("GET", "/api/workspaces"), api("GET", "/api/sessions")]);
    } catch (error) {
      state.offline += 1;
      $("#offline").hidden = state.offline < 2;
      throw error;
    }
    state.offline = 0;
    $("#offline").hidden = true;
    state.workspaces = ws.workspaces;
    state.sessions = ss.sessions;
    // A lost link whose session is gone will never come back: stop waiting.
    const attached = sessionById(state.attached);
    if (state.attached && !state.socket && !state.exited && (!attached || !attached.live)) {
      state.snapshot = JSON.stringify([ws, ss]);
      detach("That session's terminal is gone (closed, or the hub restarted).");
      return;
    }
    const snapshot = JSON.stringify([ws, ss]);
    const busy = $("#sidebar").contains(document.activeElement) &&
      document.activeElement.matches("input, select, textarea");
    if (!force && (snapshot === state.snapshot || busy)) return;
    state.snapshot = snapshot;
    render();
  }

  // --- theme -----------------------------------------------------------

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
    if (term) term.options.theme = TERM_THEMES[resolvedTheme()];
  }
  prefersLight.addEventListener("change", () => applyTheme(theme));

  // --- terminal --------------------------------------------------------

  const TERM_FONT = '"FW Term", ui-monospace, "SF Mono", Menlo, Consolas, monospace';
  let term = null;
  let fit = null;

  function createTerminal() {
    term = new window.Terminal({
      cursorBlink: true,
      fontFamily: TERM_FONT,
      fontSize: 13,
      lineHeight: 1.12,
      scrollback: 10000,
      allowProposedApi: false,
      theme: TERM_THEMES[resolvedTheme()],
    });
    fit = new window.FitAddon.FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new window.WebLinksAddon.WebLinksAddon());
    term.open($("#terminal"));
    term.onData((data) => send(JSON.stringify({ type: "input", data })));
    term.onBinary((data) => {
      const bytes = new Uint8Array(data.length);
      for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i) & 0xff;
      send(bytes);
    });
    term.onResize(({ cols, rows }) => {
      send(JSON.stringify({ type: "resize", cols, rows }));
      updateMeters();
    });
    let pending = 0;
    const refit = () => {
      cancelAnimationFrame(pending);
      pending = requestAnimationFrame(() => {
        if ($("#terminal").clientWidth > 0) fit.fit();
      });
    };
    window.addEventListener("resize", refit);
    new ResizeObserver(refit).observe($("#terminal"));
    fit.fit();
  }

  function send(message) {
    const socket = state.socket;
    if (socket && socket.readyState === WebSocket.OPEN) socket.send(message);
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

  function stopRetry() {
    clearTimeout(state.retryTimer);
    clearInterval(state.countdown);
    state.retryTimer = null;
    state.countdown = null;
  }

  function closeSocket() {
    stopRetry();
    if (state.socket) {
      state.socket.onclose = null;
      state.socket.close(1000);
      state.socket = null;
    }
  }

  function detach(message) {
    closeSocket();
    state.attached = null;
    state.exited = null;
    term.reset();
    history.replaceState(null, "", window.location.pathname);
    hideBanner();
    setLink("idle");
    state.notice = message || "";
    render();
  }

  function attach(id) {
    const session = sessionById(id);
    if (!session || !session.live) return;
    closeSocket();
    state.attached = id;
    state.exited = null;
    state.retry = 0;
    state.notice = "";
    delete state.wsErrors[session.workspace_id];
    rememberWorkspace(session.workspace_id);
    history.replaceState(null, "", `#session=${id}`);
    hideBanner();
    document.body.classList.remove("side-open");
    term.reset();
    connect(id);
    render();
  }

  // Open (or re-open) the attached session's socket. Each connect replays
  // the scrollback, so the terminal is reset first.
  function connect(id) {
    stopRetry();
    setLink("connecting", state.retry ? `retry ${state.retry}` : "connecting");
    const scheme = window.location.protocol === "https:" ? "wss" : "ws";
    const socket = new WebSocket(`${scheme}://${window.location.host}/api/sessions/${id}/ws`);
    socket.binaryType = "arraybuffer";
    state.socket = socket;
    socket.onopen = () => {
      // The replay follows; clear only now, so a dead link keeps the last
      // screen visible while it retries.
      term.reset();
      state.retry = 0;
      setLink("attached");
      hideBanner();
      fit.fit();
      send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
      term.focus();
      refresh(false).catch(() => {});
    };
    socket.onmessage = (event) => {
      if (typeof event.data !== "string") {
        term.write(new Uint8Array(event.data));
        return;
      }
      let message;
      try {
        message = JSON.parse(event.data);
      } catch (_) {
        return;
      }
      if (message.type === "reset") {
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
      if (state.exited || state.attached !== id) return;
      scheduleReconnect(id, event.code);
    };
  }

  function onExit(id, message) {
    const how = message.signal ? `killed by ${message.signal}` : `exit code ${message.code}`;
    state.exited = message;
    term.write(`\r\n\x1b[2m[process ended: ${how}]\x1b[0m\r\n`);
    setLink("exited", "ended");
    const session = sessionById(id);
    showBanner(
      `Process ended (${how}). Scrollback stays until the session is closed.`,
      [
        el("button", { type: "button", className: "slab", onclick: () => relaunch(id) }, "Relaunch"),
        el("button", { type: "button", className: "slab ghost", onclick: () => closeSession(id) }, "Close"),
      ],
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
      $("#banner-text").textContent = `${why} Reconnecting in ${seconds}s.`;
    };
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
    if (state.attached !== id) return;
    const session = sessionById(id);
    if (state.offline === 0 && (!session || !session.live)) {
      detach("That session's terminal is gone (closed, or the hub restarted).");
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

  async function relaunch(id) {
    const session = sessionById(id);
    if (!session) return;
    const body = {
      workspace_id: session.workspace_id,
      repo: session.cwd,
      kind: session.kind || "shell",
      name: session.name,
    };
    if (body.kind === "shell") body.command = session.command;
    try {
      await createSession(body);
    } catch (error) {
      showBanner(`Relaunch failed: ${error.message}`, [], false);
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
    if (session.state === "running" && session.live) {
      const ok = await confirmDialog({
        kicker: "close session",
        title: `Close ${session.name}?`,
        body: [
          "Its process",
          session.live.pid ? ` (pid ${session.live.pid})` : "",
          " is hung up and the scrollback is discarded. Every attached browser loses it.",
        ].join(""),
        ok: "Kill and close",
      });
      if (!ok) return;
    }
    try {
      await api("DELETE", `/api/sessions/${id}`);
      if (state.attached === id) detach();
      await refresh();
    } catch (error) {
      flagError(id, error);
    }
  }

  async function renameSession(id, name) {
    state.renaming = null;
    const session = sessionById(id);
    if (!session || !name.trim() || name.trim() === session.name) {
      render();
      return;
    }
    try {
      await api("PATCH", `/api/sessions/${id}`, { name });
      await refresh();
    } catch (error) {
      flagError(id, error);
      render();
    }
  }

  function startRename(id) {
    const session = sessionById(id);
    if (!session) return;
    state.collapsed.delete(session.workspace_id);
    if (window.matchMedia("(max-width: 820px)").matches) document.body.classList.add("side-open");
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
    const count = state.sessions.filter((s) => s.workspace_id === workspace.id).length;
    const ok = await confirmDialog({
      kicker: "delete workspace",
      title: `Delete ${workspace.name}?`,
      body: count
        ? `Its ${count} session record${count === 1 ? "" : "s"} go with it. Running sessions must be closed first. The repos on disk are not touched.`
        : "The repos on disk are not touched.",
      ok: "Delete",
    });
    if (!ok) return;
    try {
      await api("DELETE", `/api/workspaces/${workspace.id}`);
      delete state.wsErrors[workspace.id];
      await refresh();
    } catch (error) {
      state.wsErrors[workspace.id] = error.message;
      render();
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
          if (e.key === "Enter") renameSession(session.id, e.target.value);
          if (e.key === "Escape") {
            state.renaming = null;
            render();
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
        : null,
      iconButton("x", "Close session", () => closeSession(session.id), true),
    );
    return el(
      "li",
      {
        className: `session${active ? " active" : ""}${session.live ? "" : " gone"}`,
        dataset: { id: session.id },
        tabIndex: 0,
        role: "button",
        "aria-current": active ? "true" : undefined,
        title: session.live ? undefined : "Ended before the hub restarted; its terminal is gone",
        onclick: () => {
          if (session.live) attach(session.id);
          else {
            state.wsErrors[session.workspace_id] =
              `"${session.name}" ended before the hub restarted; its terminal is gone. Close it to clear it.`;
            render();
          }
        },
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
        el("span", { className: `dot ${st.dot}` }),
      ),
      nameNode,
      el(
        "span",
        { className: "session-meta" },
        el("span", { className: `session-state ${st.tone}`, textContent: st.label }),
        actions,
      ),
    );
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
          workspace.repos.map((r) => el("span", { textContent: basename(r), title: r })),
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
    const ordered = orderedSessions();
    const numbering = new Map(ordered.map((s, i) => [s.id, i]));
    const root = $("#workspaces");
    if (state.workspaces.length === 0) {
      root.replaceChildren(
        el(
          "div",
          { className: "no-workspaces" },
          "A workspace is a name and the git repos you work on together. Create one to start sessions.",
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

    renderMain();
  }

  function renderMain() {
    const session = sessionById(state.attached);
    const workspace = session ? workspaceById(session.workspace_id) : null;
    $("#empty").hidden = !!session;
    $("#crumb-workspace").textContent = workspace ? workspace.name : "";
    $("#crumb-session").textContent = session ? session.name : "No session";
    const kind = $("#crumb-kind");
    kind.hidden = !session;
    if (session) {
      kind.textContent = session.kind || "shell";
      kind.className = `kind-tag ${session.kind || "shell"}`;
    }
    // The LRMs keep a path's slashes in place under the start-ellipsis
    // (direction: rtl) styling.
    $("#meter-cwd").textContent = session ? `\u200e${session.cwd}\u200e` : "";
    $("#meter-cwd").title = session ? session.cwd : "";
    updateMeters();
    document.title = session ? `${session.name} · flywheel hub` : "flywheel hub";

    const actions = $("#empty-actions");
    if (!session) {
      const hasWorkspace = state.workspaces.length > 0;
      $("#empty-title").textContent = hasWorkspace ? "Nothing on the wheel." : "Set up a workspace.";
      $("#empty-body").textContent = state.notice || (hasWorkspace
        ? "Pick a session from the left, or launch a shell, Claude Code, or Codex in one of your repos."
        : "Group the git repos you work on together, then launch shells and agents inside them. Every terminal lives on the hub: close the tab, come back from your phone, it is still there.");
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
  let wsEditing = null; // workspace being edited, or null for create
  let checkSeq = 0;

  function repoRow(value) {
    const input = el("input", {
      type: "text",
      value: value || "",
      placeholder: "~/code/repo",
      autocomplete: "off",
      spellcheck: false,
      "aria-label": "Repository path",
    });
    const verdict = icon("check");
    verdict.classList.add("verdict");
    const note = el("span", { className: "note" });
    const row = el(
      "li",
      { className: "repo-row", dataset: { state: "empty" } },
      el("span", { className: "input-wrap" }, input, verdict),
      iconButton("x", "Remove repo", () => {
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
      row.result = null;
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
    row.note = note;
    if (value) {
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
    row.pending = api("GET", `/api/repos/check?path=${encodeURIComponent(raw)}`)
      .then((result) => {
        if (row.seq !== seq) return row.pending;
        row.result = result;
        row.dataset.state = result.ok ? "ok" : "bad";
        setVerdict(row, result.ok);
        row.note.textContent = result.ok
          ? (result.path !== raw ? result.path : "")
          : result.error;
        markDuplicates();
        return result;
      })
      .catch((error) => {
        if (row.seq !== seq) return row.pending;
        row.result = { ok: false, error: error.message };
        row.dataset.state = "bad";
        setVerdict(row, false);
        row.note.textContent = error.message;
        return row.result;
      });
    return row.pending;
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
        row.note.textContent = `same repo as ${pad2(first + 1)}; listed once`;
      } else {
        seen.set(row.result.path, i);
        if (row.note.textContent.startsWith("same repo as")) {
          row.note.textContent = row.result.path !== row.input.value.trim() ? row.result.path : "";
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
    if (workspace) workspace.repos.forEach((r) => addRepoRow(r));
    else addRepoRow("");
    $("#dlg-workspace").showModal();
    wsForm.name.focus();
  }

  $("#add-repo").addEventListener("click", () => addRepoRow("").input.focus());

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
      error.textContent = "Add at least one repository.";
      $("#repo-rows .repo-row input").focus();
      return;
    }
    const submit = $("#workspace-submit");
    submit.disabled = true;
    try {
      const results = await Promise.all(rows.map((row) => row.result || row.pending || row.check()));
      const bad = rows.find((row, i) => !results[i] || !results[i].ok);
      if (bad) {
        error.textContent = "Fix the highlighted repository paths.";
        bad.input.focus();
        return;
      }
      const repos = results.map((r) => r.path);
      const saved = wsEditing
        ? await api("PATCH", `/api/workspaces/${wsEditing.id}`, { name, repos })
        : await api("POST", "/api/workspaces", { name, repos });
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
    const legend = el("legend", { className: "field-label", textContent: "In repo" });
    if (!workspace) {
      picker.replaceChildren(legend);
      return;
    }
    const chosen = workspace.repos.includes(state.repoChoice[workspace.id])
      ? state.repoChoice[workspace.id]
      : workspace.repos[0];
    picker.replaceChildren(
      legend,
      ...workspace.repos.map((repo) =>
        el(
          "label",
          { className: "repo-choice" },
          el("input", { type: "radio", name: "repo", value: repo, checked: repo === chosen }),
          el("b", { textContent: basename(repo) }),
          el("small", { textContent: repo }),
        )),
    );
    updateSessionPlaceholder();
  }

  function updateSessionPlaceholder() {
    const kind = selectedKind();
    const workspaceId = $("#session-workspace").value;
    const count = state.sessions.filter((s) => s.workspace_id === workspaceId).length;
    $("#session-name").placeholder = `${kind} ${count + 1}`;
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
      submit.disabled = false;
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
    for (const [i, session] of orderedSessions().entries()) {
      if (!session.live) continue;
      const workspace = workspaceById(session.workspace_id);
      items.push({
        group: "Sessions",
        glyph: glyphFor(session.kind),
        label: session.name,
        hint: `${workspace ? workspace.name : ""} · ${status(session).label}${i < 9 ? ` · Alt ${i + 1}` : ""}`,
        run: () => attach(session.id),
      });
    }
    const cmd = (label, hint, run, glyph = "chev") => items.push({ group: "Commands", glyph, label, hint, run });
    cmd("New session", "Alt N", () => openSessionDialog(), "plus");
    for (const workspace of state.workspaces) {
      cmd(`New session in ${workspace.name}`, "", () => openSessionDialog(workspace.id), "plus");
    }
    cmd("New workspace", "Alt Shift N", () => openWorkspaceDialog(), "plus");
    if (attached) {
      cmd(`Rename ${attached.name}`, "Alt R", () => startRename(attached.id), "pen");
      if (attached.state === "running") cmd(`Stop ${attached.name}`, "keep scrollback", () => stopSession(attached.id), "stop");
      cmd(`Close ${attached.name}`, "Alt W", () => closeSession(attached.id), "x");
      cmd("Detach", "leave the session running", () => detach(), "chev");
    }
    for (const workspace of state.workspaces) {
      cmd(`Edit workspace ${workspace.name}`, "rename, repos", () => openWorkspaceDialog(workspace), "gear");
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
    const live = orderedSessions().filter((s) => s.live);
    if (!live.length) return;
    const at = live.findIndex((s) => s.id === state.attached);
    const next = at < 0 ? (delta > 0 ? 0 : live.length - 1) : (at + delta + live.length) % live.length;
    attach(live[next].id);
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
          if (state.attached) send(JSON.stringify({ type: "input", data: "\x0b" }));
        } else if (!document.querySelector("dialog[open]")) {
          openPalette();
        }
        return;
      }
      if (paletteOpen() || document.querySelector("dialog[open]")) return;
      if (!event.altKey || mod) return;
      let handled = true;
      const digit = /^Digit([1-9])$/.exec(event.code);
      if (digit) {
        const target = orderedSessions()[Number(digit[1]) - 1];
        if (target && target.live) attach(target.id);
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
    try {
      await refresh();
    } catch (error) {
      showError(error);
    }
    // Re-attach the session named in the URL (survives reloads).
    const match = /^#session=([0-9a-f]+)$/.exec(window.location.hash);
    if (match) {
      const session = sessionById(match[1]);
      if (session && session.live) attach(session.id);
      else history.replaceState(null, "", window.location.pathname);
    }
    setInterval(() => refresh(false).catch(() => {}), 4000);
  }

  boot();
})();

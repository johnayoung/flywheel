// flywheel hub: multiple machines. The other hubs this hub registered
// (GET/POST/DELETE /api/machines) show beside its own workspaces: their
// workspaces and sessions are fetched through this hub's proxy
// (/m/<machine>/api/...), each machine with its own events socket,
// connection state, and reconnect backoff -- one machine down never stalls
// the others or this hub. Ids are UUIDs, so a remote workspace or session
// keeps its own id here; `route()` sends every call naming one to its
// machine, and calls with no id (kinds, folder checks, worktree plans, a
// new workspace) to the machine the open dialog is about. app.js creates
// it with its helpers.
"use strict";

window.FwHubMachines = function (ctx) {
  const { $, el, icon, state, clientId } = ctx;

  // Reconnect backoff per machine: 0.5s doubling to 30s.
  const BACKOFF_MS = 500;
  const BACKOFF_CAP_MS = 30000;
  // Longest the first paint waits for a machine's lists.
  const FIRST_LOAD_MS = 4000;
  const ui = {
    host: "",
    started: false,
    machines: new Map(), // name -> machine runtime (below)
    owner: new Map(), // workspace / session id -> machine name
    context: null, // () => machine name the open dialog is about, or null
    version: 0, // bumped when any machine's lists or state change
  };

  const prefix = (name) => (name ? `/m/${encodeURIComponent(name)}` : "");

  // --- routing -----------------------------------------------------------

  const ID_PATH = /^\/api\/(?:sessions|workspaces)\/([0-9a-f]{8,64})(?=[/?]|$)/;
  // Calls about "where": they follow the dialog's machine.
  const CONTEXT_GET = /^\/api\/(?:kinds|folders\/check|repos\/check|worktrees\/plan)(?=[/?]|$)/;

  function route(method, path, body) {
    if (path.startsWith("/m/")) return path;
    const match = ID_PATH.exec(path);
    let machine = match ? ui.owner.get(match[1]) : null;
    if (!machine && !match && body && typeof body === "object") {
      machine = ui.owner.get(body.workspace_id) || ui.owner.get(body.session_id) || null;
    }
    if (!machine && !match && ui.context) {
      const about = ui.context();
      const create = method === "POST" && /^\/api\/workspaces$/.test(path);
      if (about && ((method === "GET" && CONTEXT_GET.test(path)) || create)) machine = about;
    }
    return machine && ui.machines.has(machine) ? prefix(machine) + path : path;
  }

  const machineOf = (id) => ui.owner.get(id) || null;

  // --- lists -------------------------------------------------------------

  function runtime(view) {
    return {
      name: view.name,
      url: view.url,
      view,
      link: view.status && view.status.state === "refused" ? "refused" : "connecting",
      error: (view.status && view.status.error) || "",
      socket: null,
      retry: 0,
      timer: null,
      due: 0,
      workspaces: [],
      sessions: [],
      loaded: false,
      agentKeys: new Set(),
      rollupKeys: new Set(),
      seq: 0,
      viewing: undefined,
    };
  }

  function bump() {
    ui.version += 1;
  }

  // The registered machines (GET /api/machines); new ones connect, removed
  // ones disconnect and drop their workspaces.
  async function sync() {
    let data;
    try {
      data = await ctx.rawApi("GET", "/api/machines");
    } catch (_) {
      return false;
    }
    ui.host = data.host || "";
    const seen = new Set();
    const fresh = [];
    for (const view of data.machines || []) {
      seen.add(view.name);
      const known = ui.machines.get(view.name);
      if (known && known.url === view.url) {
        known.view = view;
        continue;
      }
      if (known) drop(known);
      const m = runtime(view);
      ui.machines.set(view.name, m);
      fresh.push(m);
    }
    for (const m of [...ui.machines.values()]) {
      if (!seen.has(m.name)) drop(m);
    }
    bump();
    await Promise.all(fresh.map((m) => withTimeout(load(m), FIRST_LOAD_MS)));
    for (const m of fresh) connect(m);
    return true;
  }

  const withTimeout = (promise, ms) =>
    Promise.race([promise.catch(() => {}), new Promise((resolve) => setTimeout(resolve, ms))]);

  function drop(m) {
    ui.machines.delete(m.name);
    clearTimeout(m.timer);
    if (m.socket) {
      const socket = m.socket;
      m.socket = null;
      socket.close();
    }
    for (const key of m.agentKeys) delete state.agent[key];
    for (const key of m.rollupKeys) delete state.rollup[key];
    forget(m);
    bump();
  }

  function forget(m) {
    for (const [id, owner] of ui.owner) if (owner === m.name) ui.owner.delete(id);
  }

  // A machine's workspaces and sessions (kept when it cannot be reached:
  // its sessions show as on an offline machine, never as removed).
  async function load(m) {
    const seq = ++m.seq;
    let ws;
    let ss;
    try {
      [ws, ss] = await Promise.all([
        ctx.rawApi("GET", `${prefix(m.name)}/api/workspaces`),
        ctx.rawApi("GET", `${prefix(m.name)}/api/sessions`),
      ]);
    } catch (error) {
      if (seq !== m.seq || !ui.machines.has(m.name)) return;
      noteFailure(m, error);
      throw error;
    }
    if (seq !== m.seq || !ui.machines.has(m.name)) return;
    m.workspaces = ws.workspaces.map((w) => ({ ...w, machine: m.name }));
    m.sessions = ss.sessions.map((s) => ({ ...s, machine: m.name }));
    m.loaded = true;
    forget(m);
    for (const w of m.workspaces) ui.owner.set(w.id, m.name);
    for (const s of m.sessions) ui.owner.set(s.id, m.name);
    if (m.link !== "online" && m.socket && m.socket.readyState === WebSocket.OPEN) setLink(m, "online", "");
    bump();
  }

  function noteFailure(m, error) {
    const refused = error && error.machineState === "refused";
    setLink(m, refused ? "refused" : "offline", error ? error.message : "");
  }

  function setLink(m, link, error) {
    if (m.link === link && m.error === error) return;
    m.link = link;
    m.error = error || "";
    bump();
    ctx.onChange();
  }

  // Every machine's workspaces and sessions, in machine order.
  const workspaces = () => [...ui.machines.values()].flatMap((m) => m.workspaces);
  const sessions = () => [...ui.machines.values()].flatMap((m) => m.sessions);

  // Re-read one machine's lists, then repaint.
  async function reload(name) {
    const m = ui.machines.get(name);
    if (!m) return;
    try {
      await load(m);
    } catch (_) {
      // Its state says why; the lists stay as they were.
    }
    ctx.onChange();
  }

  // Every reachable machine's lists (after an action here: a session made
  // or renamed there shows at once). Bounded per machine.
  function reloadAll() {
    const live = [...ui.machines.values()].filter((m) => m.link === "online" || m.link === "connecting");
    return Promise.all(live.map((m) => withTimeout(load(m), FIRST_LOAD_MS)));
  }

  // Per-workspace token usage on every reachable machine (GET
  // /api/usage there): its workspaces' rollups.
  async function usage() {
    const live = [...ui.machines.values()].filter((m) => m.link === "online");
    const lists = await Promise.all(live.map((m) =>
      ctx.rawApi("GET", `${prefix(m.name)}/api/usage`).then((u) => u.workspaces || [], () => [])));
    return lists.flat();
  }

  // --- events, one socket per machine --------------------------------------

  function connect(m) {
    clearTimeout(m.timer);
    if (!ui.machines.has(m.name) || m.socket) return;
    const scheme = window.location.protocol === "https:" ? "wss" : "ws";
    const socket = new WebSocket(
      `${scheme}://${window.location.host}${prefix(m.name)}/api/events?client=${encodeURIComponent(clientId)}`,
    );
    m.socket = socket;
    if (m.link !== "refused") setLink(m, "connecting", m.error);
    socket.onopen = () => {
      if (m.socket !== socket) return;
      m.retry = 0;
      m.viewing = undefined;
      setLink(m, "online", "");
      sendView(true);
      reload(m.name);
      ctx.onMachineFlywheel(m.name);
    };
    socket.onmessage = (event) => {
      if (m.socket !== socket) return;
      let message;
      try {
        message = JSON.parse(event.data);
      } catch (_) {
        return;
      }
      onMessage(m, message);
    };
    socket.onclose = (event) => {
      if (m.socket !== socket) return;
      m.socket = null;
      m.viewing = undefined;
      if (!ui.machines.has(m.name)) return;
      // 4001/4002: this hub's own sign-in ended (the proxy's credential).
      if (event.code === 4001 || event.code === 4002) {
        ctx.signedOut(event.code === 4001 ? "rotated" : "ended");
        return;
      }
      if (event.code === 4011) {
        sync().then(() => ctx.onChange());
        return;
      }
      if (event.code === 4010) {
        setLink(m, "refused", `${m.name} refused this hub's credential (revoked there?). Remove and re-add the machine.`);
      } else if (event.code === 1012) {
        // That hub is being upgraded: back in a moment.
        m.timer = setTimeout(() => connect(m), 300);
        return;
      } else {
        setLink(m, "offline", m.error || `cannot reach ${m.name}`);
      }
      // A refused credential stays refused: retry slowly.
      m.retry += 1;
      const floor = m.link === "refused" ? BACKOFF_CAP_MS : 0;
      const delay = Math.max(floor, Math.min(BACKOFF_CAP_MS, BACKOFF_MS * 2 ** Math.min(m.retry - 1, 6)));
      m.due = Date.now() + delay;
      m.timer = setTimeout(() => {
        // The lists too (the failure that closed it may have been a blip).
        reload(m.name);
        connect(m);
      }, delay);
      ctx.onChange();
    };
  }

  function reconnect(name) {
    const m = ui.machines.get(name);
    if (!m) return;
    m.retry = 0;
    clearTimeout(m.timer);
    if (m.socket) return;
    reload(name);
    connect(m);
  }

  // Wake every machine that is waiting out a backoff (online, focus).
  function wake() {
    for (const m of ui.machines.values()) {
      if (!m.socket && m.link !== "refused") reconnect(m.name);
    }
  }

  function onMessage(m, message) {
    if (message.type === "snapshot") {
      for (const key of m.agentKeys) delete state.agent[key];
      for (const key of m.rollupKeys) delete state.rollup[key];
      m.agentKeys = new Set(Object.keys(message.sessions || {}));
      m.rollupKeys = new Set(Object.keys(message.workspaces || {}));
      Object.assign(state.agent, message.sessions || {});
      Object.assign(state.rollup, message.workspaces || {});
      ctx.onStatusChange();
    } else if (message.type === "status") {
      ctx.onStatus(message);
      if (message.status) m.agentKeys.add(message.session_id);
      else m.agentKeys.delete(message.session_id);
      if (message.workspace_id) {
        if (message.workspace_status) m.rollupKeys.add(message.workspace_id);
        else m.rollupKeys.delete(message.workspace_id);
      }
    } else if (message.type === "sessions") {
      reload(m.name);
    } else {
      // commands, usage, flywheel, layout: as the hub's own.
      ctx.onEvent(message, m.name);
    }
  }

  // What this page shows, told to each machine about its own sessions
  // (they count as seen there).
  function sendView(force) {
    const view = ctx.currentView();
    for (const m of ui.machines.values()) {
      const socket = m.socket;
      if (!socket || socket.readyState !== WebSocket.OPEN) continue;
      const mine = {
        session_id: view.session_id && ui.owner.get(view.session_id) === m.name ? view.session_id : null,
        visible: view.visible.filter((id) => ui.owner.get(id) === m.name),
      };
      const key = JSON.stringify(mine);
      if (!force && key === m.viewing) continue;
      m.viewing = key;
      socket.send(JSON.stringify({ type: "view", ...mine }));
    }
  }

  // --- the sidebar ---------------------------------------------------------

  const LINK_TEXT = {
    connecting: "connecting",
    online: "online",
    offline: "offline",
    refused: "refused",
  };

  // A machine group's head: its name, connection state, reconnect.
  function header(name) {
    if (!name) {
      return el(
        "div",
        { className: "machine-head local" },
        el("span", { className: "machine-dot link-online", "aria-hidden": "true" }),
        el("span", { className: "machine-name", textContent: ui.host || "this machine" }),
        el("span", { className: "machine-state", textContent: "this hub" }),
      );
    }
    const m = ui.machines.get(name);
    const link = m ? m.link : "offline";
    const waiting = m && !m.socket && m.due > Date.now() ? Math.ceil((m.due - Date.now()) / 1000) : 0;
    return el(
      "div",
      {
        className: `machine-head link-${link}`,
        title: [m ? m.url : "", m && m.error ? m.error : ""].filter(Boolean).join("\n"),
      },
      el("span", { className: `machine-dot link-${link}`, "aria-hidden": "true" }),
      el("span", { className: "machine-name", textContent: name }),
      el("span", {
        className: "machine-state",
        textContent: waiting && link !== "online" ? `${LINK_TEXT[link]} · retry ${waiting}s` : LINK_TEXT[link],
      }),
      link === "online" || link === "connecting"
        ? null
        : el(
          "button",
          {
            type: "button",
            className: "text-btn machine-retry",
            title: `Reconnect to ${name} now`,
            onclick: (event) => {
              event.stopPropagation();
              reconnect(name);
            },
          },
          "reconnect",
        ),
    );
  }

  // Countdowns tick while a machine waits.
  setInterval(() => {
    if (document.visibilityState !== "visible") return;
    if ([...ui.machines.values()].some((m) => !m.socket && m.due > Date.now())) ctx.onChange();
  }, 1000);

  const names = () => [...ui.machines.keys()];
  const any = () => ui.machines.size > 0;
  const linkOf = (name) => (ui.machines.get(name) || {}).link || null;

  // --- the machines dialog ----------------------------------------------------

  function renderList() {
    const list = $("#machine-list");
    const rows = [...ui.machines.values()].map((m) =>
      el(
        "li",
        { className: `machine-row link-${m.link}` },
        el("span", { className: `machine-dot link-${m.link}`, "aria-hidden": "true" }),
        el(
          "span",
          { className: "machine-row-text" },
          el("b", { textContent: m.name }),
          el("small", { textContent: m.url }),
          m.view.peer ? el("small", { textContent: `peer ${m.view.peer.name} (${m.view.peer.id}) there` }) : null,
          m.view.fingerprint ? el("small", { className: "machine-fp", textContent: `pinned ${m.view.fingerprint}` }) : null,
          m.error ? el("small", { className: "machine-err", textContent: m.error }) : null,
        ),
        el(
          "span",
          { className: "machine-row-tools" },
          el("button", {
            type: "button",
            className: "slab ghost",
            textContent: "Test",
            onclick: () => test(m.name),
          }),
          el("button", {
            type: "button",
            className: "slab ghost danger",
            textContent: "Remove",
            onclick: () => remove(m.name),
          }),
        ),
      ));
    list.replaceChildren(
      ...(rows.length
        ? rows
        : [el("li", { className: "machine-none", textContent: "No other machines yet." })]),
    );
    $("#machine-host").textContent = ui.host || "this hub";
  }

  async function test(name) {
    const m = ui.machines.get(name);
    try {
      const result = await ctx.rawApi("POST", `/api/machines/${encodeURIComponent(name)}/test`);
      if (m) {
        m.view = result.machine || m.view;
        if (result.ok) {
          setLink(m, m.socket ? "online" : m.link, "");
          ctx.toast(`${name}: ${result.latency_ms} ms, flywheel ${result.whoami.version} on ${result.whoami.host}`, { timeout: 3500 });
          if (!m.socket) reconnect(name);
        } else {
          const state = result.machine && result.machine.status ? result.machine.status.state : "offline";
          setLink(m, state === "refused" ? "refused" : "offline", result.error);
          ctx.toast(`${name}: ${result.error}`, { timeout: 6000 });
        }
      }
    } catch (error) {
      ctx.toast(`${name}: ${error.message}`, { timeout: 5000 });
    }
    renderList();
  }

  async function remove(name) {
    const ok = await ctx.confirmDialog({
      kicker: "machine",
      title: `Remove ${name}?`,
      body: `Its workspaces and sessions leave this hub's view (they keep running there), and ${name} is asked to revoke this hub's credential.`,
      ok: "Remove",
    });
    if (!ok) return;
    try {
      const done = await ctx.rawApi("DELETE", `/api/machines/${encodeURIComponent(name)}`);
      if (done.note) ctx.toast(done.note, { timeout: 8000 });
    } catch (error) {
      ctx.toast(error.message, { timeout: 5000 });
    }
    await sync();
    renderList();
    ctx.onChange();
  }

  async function add(event) {
    event.preventDefault();
    const name = $("#machine-name").value.trim();
    const url = $("#machine-url").value.trim();
    const secret = $("#machine-secret").value.trim();
    const fingerprint = $("#machine-fp").value.trim();
    const mint = $("#machine-mint").checked;
    const error = $("#machine-error");
    error.textContent = "";
    if (!name || !url || !secret) {
      error.textContent = "Name, URL, and a credential are needed.";
      return;
    }
    const body = { name, url };
    if (mint) body.token = secret;
    else body.credential = secret;
    if (fingerprint) body.fingerprint = fingerprint;
    const submit = $("#machine-submit");
    submit.disabled = true;
    try {
      await ctx.rawApi("POST", "/api/machines", body);
      // The secret leaves the page at once.
      $("#machine-secret").value = "";
      $("#machine-name").value = "";
      $("#machine-url").value = "";
      $("#machine-fp").value = "";
      $("#machine-mint").checked = false;
      await sync();
      renderList();
      ctx.onChange();
      ctx.toast(`Added ${name}.`, { timeout: 2500 });
    } catch (e) {
      error.textContent = e.message;
    } finally {
      submit.disabled = false;
    }
  }

  function openDialog() {
    $("#machine-error").textContent = "";
    renderList();
    $("#dlg-machines").showModal();
    sync().then(renderList);
  }

  $("#machine-form").addEventListener("submit", add);

  // --- start -------------------------------------------------------------------

  async function start() {
    if (ui.started) return;
    ui.started = true;
    await sync();
  }

  return {
    start,
    sync,
    route,
    prefix,
    machineOf,
    workspaces,
    sessions,
    reload,
    reloadAll,
    usage,
    reconnect,
    wake,
    sendView,
    header,
    names,
    any,
    linkOf,
    openDialog,
    setContext: (fn) => {
      ui.context = fn;
    },
    version: () => ui.version,
    host: () => ui.host,
  };
};

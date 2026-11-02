// flywheel hub web client: workspaces, sessions, and one attached terminal.
// Plain JS, no build step. Talks to the hub's REST API and per-session
// WebSocket (see crates/flywheel-hub/src/server.rs for the protocol).
"use strict";

(function () {
  const $ = (sel) => document.querySelector(sel);
  const el = (tag, props, ...children) => {
    const node = document.createElement(tag);
    Object.assign(node, props || {});
    for (const child of children) {
      if (child !== null && child !== undefined) node.append(child);
    }
    return node;
  };

  const state = {
    workspaces: [],
    sessions: [],
    attached: null, // session id
    socket: null,
    exited: false, // the attached session's process has ended
    retry: 0, // reconnect attempts since the last successful attach
    retryTimer: null,
    repoChoice: {}, // workspace id -> repo picked for "+ shell"
    wsErrors: {}, // workspace id -> last action error
    snapshot: "", // last rendered data, to skip no-op re-renders
  };

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
    const data = text ? JSON.parse(text) : null;
    if (!response.ok) throw new Error((data && data.error) || response.statusText);
    return data;
  }

  // `force` re-renders even when nothing changed. The periodic refresh
  // passes false and also holds off while a sidebar control has focus, so
  // an open dropdown or a half-made choice is never torn down under the user.
  async function refresh(force = true) {
    const [ws, ss] = await Promise.all([
      api("GET", "/api/workspaces"),
      api("GET", "/api/sessions"),
    ]);
    state.workspaces = ws.workspaces;
    state.sessions = ss.sessions;
    const snapshot = JSON.stringify([ws, ss]);
    const busy = $("#sidebar").contains(document.activeElement) &&
      document.activeElement.matches("select, input, textarea");
    if (!force && (snapshot === state.snapshot || busy)) return;
    state.snapshot = snapshot;
    render();
  }

  // --- terminal --------------------------------------------------------

  const term = new window.Terminal({
    cursorBlink: true,
    fontFamily: getComputedStyle(document.body).getPropertyValue("--mono"),
    fontSize: 13,
    scrollback: 10000,
    theme: { background: "#0e1013", foreground: "#d7dae0", cursor: "#e8b45a" },
  });
  const fit = new window.FitAddon.FitAddon();
  term.loadAddon(fit);
  term.loadAddon(new window.WebLinksAddon.WebLinksAddon());
  term.open($("#terminal"));

  const send = (message) => {
    const socket = state.socket;
    if (socket && socket.readyState === WebSocket.OPEN) socket.send(message);
  };
  term.onData((data) => send(JSON.stringify({ type: "input", data })));
  term.onBinary((data) => {
    const bytes = new Uint8Array(data.length);
    for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i) & 0xff;
    send(bytes);
  });
  term.onResize(({ cols, rows }) => send(JSON.stringify({ type: "resize", cols, rows })));

  const refit = () => {
    if (state.attached) fit.fit();
  };
  window.addEventListener("resize", refit);
  new ResizeObserver(refit).observe($("#terminal"));

  function closeSocket() {
    clearTimeout(state.retryTimer);
    state.retryTimer = null;
    if (state.socket) {
      state.socket.onclose = null;
      state.socket.close(1000);
      state.socket = null;
    }
  }

  function detach(message) {
    closeSocket();
    state.attached = null;
    term.reset();
    history.replaceState(null, "", window.location.pathname);
    $("#empty").hidden = false;
    $("#term-title").textContent = "No session attached";
    $("#term-status").textContent = message || "";
    render();
  }

  function attach(id) {
    closeSocket();
    state.attached = id;
    state.exited = false;
    state.retry = 0;
    history.replaceState(null, "", `#session=${id}`);
    $("#empty").hidden = true;
    const session = state.sessions.find((s) => s.id === id);
    $("#term-title").textContent = session ? `${session.name}  ${session.cwd}` : id;
    connect(id);
    render();
  }

  // Open (or re-open) the attached session's socket. Each connect replays
  // the scrollback, so the terminal is reset first.
  function connect(id) {
    term.reset();
    $("#term-status").textContent = state.retry ? `reconnecting (${state.retry})` : "connecting";
    const scheme = window.location.protocol === "https:" ? "wss" : "ws";
    const socket = new WebSocket(`${scheme}://${window.location.host}/api/sessions/${id}/ws`);
    socket.binaryType = "arraybuffer";
    state.socket = socket;
    socket.onopen = () => {
      state.retry = 0;
      $("#term-status").textContent = "attached";
      fit.fit();
      send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
      term.focus();
    };
    socket.onmessage = (event) => {
      if (typeof event.data !== "string") {
        term.write(new Uint8Array(event.data));
        return;
      }
      const message = JSON.parse(event.data);
      if (message.type === "reset") {
        term.reset();
      } else if (message.type === "exit") {
        const how = message.signal ? `signal ${message.signal}` : `code ${message.code}`;
        term.write(`\r\n\x1b[2m[process exited with ${how}]\x1b[0m\r\n`);
        $("#term-status").textContent = `exited (${how})`;
        state.exited = true;
        refresh().catch(showError);
      } else if (message.type === "error") {
        $("#term-status").textContent = message.message;
      }
    };
    socket.onclose = (event) => {
      if (state.socket !== socket) return;
      state.socket = null;
      // 1000 is the server's own goodbye (hub shutting down) or our close.
      if (state.exited || state.attached !== id) {
        if (!state.exited) $("#term-status").textContent = "disconnected";
        return;
      }
      state.retry += 1;
      const delay = Math.min(10000, 500 * 2 ** Math.min(state.retry - 1, 5));
      $("#term-status").textContent = `disconnected (code ${event.code}); retrying`;
      state.retryTimer = setTimeout(async () => {
        try {
          await refresh();
        } catch (_) {
          // Hub unreachable: keep retrying the socket.
        }
        const session = state.sessions.find((s) => s.id === id);
        if (state.attached !== id) return;
        if (state.sessions.length && (!session || !session.live)) {
          detach("that session's terminal is gone (removed, or the hub restarted)");
          return;
        }
        connect(id);
      }, delay);
    };
  }

  // --- rendering -------------------------------------------------------

  function stateLabel(session) {
    if (session.state === "running") return "running";
    if (session.state === "exited") {
      return session.signal ? `killed (${session.signal})` : `exit ${session.code}`;
    }
    return "stopped";
  }

  function iconButton(label, title, danger) {
    return el("button", {
      type: "button",
      className: `icon${danger ? " danger" : ""}`,
      textContent: label,
      title,
      ariaLabel: title,
    });
  }

  function renderWorkspace(workspace) {
    const repoSelect = el("select", { title: "Repo for the new shell" });
    for (const repo of workspace.repos) {
      repoSelect.append(el("option", { value: repo, textContent: repo.split("/").pop() || repo }));
    }
    const chosen = state.repoChoice[workspace.id];
    if (chosen && workspace.repos.includes(chosen)) repoSelect.value = chosen;
    repoSelect.addEventListener("change", () => {
      state.repoChoice[workspace.id] = repoSelect.value;
    });
    const wsError = el("p", { className: "form-error", role: "alert", textContent: state.wsErrors[workspace.id] || "" });
    const fail = (error) => {
      state.wsErrors[workspace.id] = error.message;
      wsError.textContent = error.message;
    };
    const clear = () => {
      delete state.wsErrors[workspace.id];
      wsError.textContent = "";
    };
    const newShell = el("button", { type: "button", textContent: "+ shell" });
    newShell.addEventListener("click", async () => {
      clear();
      try {
        const created = await api("POST", "/api/sessions", {
          workspace_id: workspace.id,
          repo: repoSelect.value,
          cols: term.cols,
          rows: term.rows,
        });
        await refresh();
        attach(created.id);
      } catch (error) {
        fail(error);
      }
    });
    const remove = iconButton("\u00d7", "Delete workspace", true);
    remove.addEventListener("click", async () => {
      if (!window.confirm(`Delete workspace "${workspace.name}"?`)) return;
      clear();
      try {
        await api("DELETE", `/api/workspaces/${workspace.id}`);
        delete state.repoChoice[workspace.id];
        await refresh();
      } catch (error) {
        fail(error);
      }
    });

    const sessions = el("ul", { className: "sessions" });
    for (const session of state.sessions.filter((s) => s.workspace_id === workspace.id)) {
      const running = session.state === "running";
      const actions = el("span", { className: "session-actions" });
      if (running) {
        const stop = iconButton("\u25a0", "Stop (keep its scrollback)", false);
        stop.addEventListener("click", async (event) => {
          event.stopPropagation();
          clear();
          try {
            await api("POST", `/api/sessions/${session.id}/kill`);
            await refresh();
          } catch (error) {
            fail(error);
          }
        });
        actions.append(stop);
      }
      const forget = iconButton("\u00d7", running ? "Kill and remove" : "Remove", true);
      forget.addEventListener("click", async (event) => {
        event.stopPropagation();
        clear();
        try {
          await api("DELETE", `/api/sessions/${session.id}`);
          if (state.attached === session.id) detach();
          await refresh();
        } catch (error) {
          fail(error);
        }
      });
      actions.append(forget);
      const item = el(
        "li",
        { className: `session${state.attached === session.id ? " active" : ""}${session.live ? "" : " gone"}` },
        el("span", { className: `dot ${session.state}` }),
        el("span", { className: "session-name", textContent: session.name }),
        el("span", { className: "session-state", textContent: stateLabel(session) }),
        actions,
      );
      if (session.live) {
        item.addEventListener("click", () => attach(session.id));
      } else {
        item.title = "This session ended before the hub restarted; its terminal is gone";
        item.addEventListener("click", () => {
          wsError.textContent = `"${session.name}" ended before the hub restarted; its terminal is gone. Remove it with \u00d7.`;
        });
      }
      sessions.append(item);
    }

    return el(
      "div",
      { className: "workspace" },
      el("div", { className: "ws-head" }, el("span", { className: "ws-name", textContent: workspace.name }), remove),
      el("div", { className: "ws-repos" }, ...workspace.repos.map((r) => el("div", { textContent: r, title: r }))),
      el("div", { className: "ws-actions" }, repoSelect, newShell),
      wsError,
      sessions,
    );
  }

  function render() {
    const root = $("#workspaces");
    root.replaceChildren(...state.workspaces.map(renderWorkspace));
    if (state.workspaces.length === 0) $("#new-workspace").open = true;
    $("#empty").textContent = state.workspaces.length
      ? "Pick a session, or open a shell with + shell."
      : "Create a workspace, then open a shell in one of its repos.";
  }

  function showError(error) {
    $("#term-status").textContent = `error: ${error.message}`;
  }

  // --- forms -----------------------------------------------------------

  $("#workspace-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = event.target;
    const repos = form.repos.value.split("\n").map((r) => r.trim()).filter(Boolean);
    $("#workspace-error").textContent = "";
    try {
      await api("POST", "/api/workspaces", { name: form.name.value, repos });
      form.reset();
      $("#new-workspace").open = false;
      await refresh();
    } catch (error) {
      $("#workspace-error").textContent = error.message;
    }
  });

  $("#logout").addEventListener("click", async () => {
    await fetch("/logout", { method: "POST", credentials: "same-origin" });
    window.location.replace("/");
  });

  // Re-attach the session named in the URL (survives reloads).
  refresh()
    .then(() => {
      const match = /^#session=([0-9a-f]+)$/.exec(window.location.hash);
      if (!match) return;
      const session = state.sessions.find((s) => s.id === match[1]);
      if (session && session.live) attach(session.id);
      else history.replaceState(null, "", window.location.pathname);
    })
    .catch(showError);
  setInterval(() => refresh(false).catch(() => {}), 5000);
})();

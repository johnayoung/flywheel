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

  async function refresh() {
    const [ws, ss] = await Promise.all([
      api("GET", "/api/workspaces"),
      api("GET", "/api/sessions"),
    ]);
    state.workspaces = ws.workspaces;
    state.sessions = ss.sessions;
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

  function attach(id) {
    if (state.socket) {
      state.socket.onclose = null;
      state.socket.close();
      state.socket = null;
    }
    state.attached = id;
    term.reset();
    $("#empty").hidden = true;
    const session = state.sessions.find((s) => s.id === id);
    $("#term-title").textContent = session ? `${session.name}  ${session.cwd}` : id;
    $("#term-status").textContent = "connecting";
    const scheme = window.location.protocol === "https:" ? "wss" : "ws";
    const socket = new WebSocket(`${scheme}://${window.location.host}/api/sessions/${id}/ws`);
    socket.binaryType = "arraybuffer";
    state.socket = socket;
    socket.onopen = () => {
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
        refresh().catch(showError);
      } else if (message.type === "error") {
        $("#term-status").textContent = message.message;
      }
    };
    socket.onclose = () => {
      if (state.socket === socket) {
        state.socket = null;
        $("#term-status").textContent = "disconnected";
      }
    };
    render();
  }

  // --- rendering -------------------------------------------------------

  function stateLabel(session) {
    if (session.state === "running") return "running";
    if (session.state === "exited") {
      return session.signal ? `killed (${session.signal})` : `exit ${session.code}`;
    }
    return "stopped";
  }

  function renderWorkspace(workspace) {
    const repoSelect = el("select", { title: "Repo for the new shell" });
    for (const repo of workspace.repos) {
      repoSelect.append(el("option", { value: repo, textContent: repo.split("/").pop() || repo }));
    }
    const newShell = el("button", { type: "button", textContent: "+ shell" });
    newShell.addEventListener("click", async () => {
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
        showError(error);
      }
    });
    const remove = el("button", { type: "button", className: "icon", textContent: "x", title: "Delete workspace" });
    remove.addEventListener("click", async () => {
      if (!window.confirm(`Delete workspace "${workspace.name}"?`)) return;
      try {
        await api("DELETE", `/api/workspaces/${workspace.id}`);
        await refresh();
      } catch (error) {
        showError(error);
      }
    });

    const sessions = el("ul", { className: "sessions" });
    for (const session of state.sessions.filter((s) => s.workspace_id === workspace.id)) {
      const kill = el("button", {
        type: "button",
        className: "icon",
        textContent: "x",
        title: session.state === "running" ? "Kill and remove" : "Remove",
      });
      kill.addEventListener("click", async (event) => {
        event.stopPropagation();
        try {
          await api("DELETE", `/api/sessions/${session.id}`);
          if (state.attached === session.id) {
            if (state.socket) state.socket.close();
            state.attached = null;
            term.reset();
            $("#empty").hidden = false;
            $("#term-title").textContent = "No session attached";
            $("#term-status").textContent = "";
          }
          await refresh();
        } catch (error) {
          showError(error);
        }
      });
      const item = el(
        "li",
        { className: `session${state.attached === session.id ? " active" : ""}` },
        el("span", { className: `dot ${session.state}` }),
        el("span", { className: "session-name", textContent: session.name }),
        el("span", { className: "session-state", textContent: stateLabel(session) }),
        kill,
      );
      if (session.live) item.addEventListener("click", () => attach(session.id));
      else item.title = "This session ended before the hub restarted";
      sessions.append(item);
    }

    return el(
      "div",
      { className: "workspace" },
      el("div", { className: "ws-head" }, el("span", { className: "ws-name", textContent: workspace.name }), remove),
      el("div", { className: "ws-repos" }, ...workspace.repos.map((r) => el("div", { textContent: r, title: r }))),
      el("div", { className: "ws-actions" }, repoSelect, newShell),
      sessions,
    );
  }

  function render() {
    const root = $("#workspaces");
    root.replaceChildren(...state.workspaces.map(renderWorkspace));
    if (state.workspaces.length === 0) $("#new-workspace").open = true;
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

  refresh().catch(showError);
  setInterval(() => refresh().catch(() => {}), 5000);
})();

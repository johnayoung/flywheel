// flywheel hub: the Commands drawer. Code blocks an agent suggested, read
// by the hub from the agent's transcript (exact text, no terminal margins
// or wrapping), with Copy, Send (paste into a shell session, never Enter)
// and Send + Run. app.js creates it with its helpers; protocol:
// GET /api/sessions/{id}/commands, POST /api/sessions/{id}/paste, and the
// events socket's {"type":"commands"} pushes.
"use strict";

window.FwHubCommands = function (ctx) {
  const { $, el, icon, api, state, sessionById, workspaceById, focusTerminal } = ctx;

  const MAX_CACHED = 500;
  const cache = {}; // session id -> commands, newest first
  const ui = {
    open: false,
    source: null, // session id whose commands are shown (chosen by the user)
    target: null, // session id Send pastes into (chosen by the user)
    all: false, // every block, not only shell commands
    loading: null, // session id being fetched
    error: "",
    unseen: 0, // shell commands that arrived while the drawer was closed
  };

  const isAgent = (s) => s && (s.kind === "claude" || s.kind === "codex");
  const isRunning = (s) => s && s.live && s.live.pid && s.state === "running";
  const attachedSession = () => sessionById(state.attached);

  // --- clipboard -------------------------------------------------------

  // Secure contexts (https, localhost) get the async clipboard API. Plain
  // http (a LAN bind) has none, so a hidden textarea is selected and copied
  // with execCommand, which also works in iOS Safari as long as it runs in
  // the tap's own handler (it does: nothing is awaited before it).
  function legacyCopy(text) {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.setAttribute("aria-hidden", "true");
    // 16px: iOS zooms into smaller focused fields.
    area.style.cssText =
      "position:fixed;top:0;left:0;width:1px;height:1px;padding:0;border:0;opacity:0;font-size:16px;white-space:pre;";
    const active = document.activeElement;
    document.body.append(area);
    area.focus({ preventScroll: true });
    area.select();
    area.setSelectionRange(0, text.length);
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch (_) {
      ok = false;
    }
    area.remove();
    if (active && typeof active.focus === "function") active.focus({ preventScroll: true });
    return ok;
  }

  function copyText(text) {
    if (window.isSecureContext && navigator.clipboard && navigator.clipboard.writeText) {
      return navigator.clipboard.writeText(text).then(
        () => true,
        () => legacyCopy(text),
      );
    }
    return Promise.resolve(legacyCopy(text));
  }

  // --- toasts ----------------------------------------------------------

  function toast(message, { actions = [], timeout = 4500, tone = "" } = {}) {
    const root = $("#toasts");
    const node = el(
      "div",
      { className: `toast${tone ? ` ${tone}` : ""}` },
      el("div", { className: "toast-body" }, message),
      actions.length ? el("div", { className: "toast-actions" }, actions) : null,
    );
    const dismiss = () => {
      node.classList.add("leaving");
      setTimeout(() => node.remove(), 180);
    };
    node.addEventListener("click", (event) => {
      if (event.target.closest("button")) setTimeout(dismiss, 0);
    });
    root.append(node);
    while (root.children.length > 3) root.firstElementChild.remove();
    if (timeout) setTimeout(dismiss, timeout);
    return node;
  }

  function preview(text, max = 72) {
    const first = text.split("\n").find((l) => l.trim()) || "";
    const lines = text.split("\n").length;
    const cut = first.length > max ? `${first.slice(0, max - 1)}…` : first;
    return lines > 1 ? `${cut}  (+${lines - 1} line${lines === 2 ? "" : "s"})` : cut;
  }

  async function copyCommand(command, where) {
    const ok = await copyText(command.command);
    if (ok) {
      toast(el("span", {}, "Copied ", el("b", { textContent: `${command.command.length} chars` }), where ? ` from ${where}` : ""), {
        timeout: 2200,
      });
    } else {
      toast("Copy failed: this browser refused the clipboard. Select the text in the drawer instead.", { tone: "bad" });
    }
    return ok;
  }

  // --- data --------------------------------------------------------------

  function remember(id, incoming) {
    const known = cache[id] || [];
    const seen = new Set(known.map((c) => c.seq));
    const fresh = incoming.filter((c) => !seen.has(c.seq));
    cache[id] = fresh.concat(known).sort((a, b) => b.seq - a.seq).slice(0, MAX_CACHED);
    return fresh;
  }

  async function load(id) {
    ui.loading = id;
    ui.error = "";
    renderDrawer();
    try {
      const list = await api("GET", `/api/sessions/${id}/commands?all=true&limit=${MAX_CACHED}`);
      cache[id] = list.commands;
    } catch (error) {
      ui.error = error.message;
      // Not retried by every repaint; opening the drawer reloads.
      if (!cache[id]) cache[id] = [];
    } finally {
      if (ui.loading === id) ui.loading = null;
      renderDrawer();
    }
  }

  // Agent sessions the drawer can show: the attached session's workspace
  // (every workspace when nothing is attached).
  function sources() {
    const attached = attachedSession();
    return state.sessions.filter(
      (s) => isAgent(s) && (!attached || s.workspace_id === attached.workspace_id),
    );
  }

  function newestSeqTime(id) {
    const list = cache[id];
    if (!list || !list.length) return "";
    return list[0].timestamp || "";
  }

  // The attached agent session; else the user's pick; else the agent in
  // this workspace that suggested something most recently.
  function currentSource() {
    const attached = attachedSession();
    if (isAgent(attached)) return attached;
    const pool = sources();
    const picked = pool.find((s) => s.id === ui.source);
    if (picked) return picked;
    return pool.slice().sort((a, b) => (newestSeqTime(b.id) > newestSeqTime(a.id) ? 1 : -1))[0] || null;
  }

  // Running shell sessions of the source's workspace.
  function targets(source) {
    const workspace = source ? source.workspace_id : state.attached && attachedSession().workspace_id;
    return state.sessions.filter((s) => s.kind === "shell" && isRunning(s) && s.workspace_id === workspace);
  }

  function currentTarget(source) {
    const pool = targets(source);
    const attached = attachedSession();
    return (
      pool.find((s) => s.id === ui.target) ||
      (attached && pool.find((s) => s.id === attached.id)) ||
      pool[0] ||
      null
    );
  }

  function shown(source) {
    const list = (source && cache[source.id]) || [];
    return ui.all ? list : list.filter((c) => c.shell);
  }

  // --- actions -----------------------------------------------------------

  async function sendTo(command, run, source) {
    const target = currentTarget(source);
    if (!target) {
      toast("No running shell in this workspace to send to.", { tone: "bad" });
      return;
    }
    try {
      await api("POST", `/api/sessions/${target.id}/paste`, { text: command.command, run });
      const open = target.id !== state.attached
        ? [el("button", { type: "button", className: "text-btn", textContent: "open", onclick: () => ctx.openSession(target.id) })]
        : [];
      toast(`${run ? "Ran" : "Pasted"} in ${target.name}${run ? "" : " (not run: press Enter there)"}.`, {
        actions: open,
        timeout: 3000,
      });
      if (target.id === state.attached) focusTerminal();
    } catch (error) {
      toast(error.message, { tone: "bad", timeout: 6000 });
    }
  }

  function copyLast() {
    const source = currentSource();
    const list = source ? (cache[source.id] || []).filter((c) => c.shell) : [];
    if (!list.length) {
      // Nothing cached yet: fetch, then the next press copies (the
      // clipboard needs the key press itself).
      if (source && !cache[source.id]) load(source.id);
      toast(source ? `${source.name} has suggested no shell commands yet.` : "No agent session here.", {
        timeout: 2500,
      });
      return;
    }
    copyCommand(list[0], source.name);
  }

  // --- drawer ------------------------------------------------------------

  function when(timestamp) {
    if (!timestamp) return "";
    const at = new Date(timestamp);
    if (Number.isNaN(at.getTime())) return "";
    const seconds = Math.max(0, (Date.now() - at.getTime()) / 1000);
    if (seconds < 45) return "just now";
    if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
    if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
    return at.toLocaleDateString();
  }

  function renderItem(command, source, target) {
    const lang = command.lang || (command.shell ? "shell" : "text");
    const lines = command.text.split("\n").length;
    return el(
      "li",
      { className: `cmd${command.shell ? " shell" : ""}`, dataset: { seq: String(command.seq) } },
      el(
        "div",
        { className: "cmd-meta" },
        el("span", { className: "cmd-lang", textContent: lang }),
        el("span", { className: "cmd-when", textContent: when(command.timestamp), title: command.timestamp || "" }),
        el("span", { className: "cmd-lines", textContent: `${lines} line${lines === 1 ? "" : "s"}` }),
        command.command !== command.text
          ? el("span", { className: "cmd-note", textContent: "prompts stripped", title: "Copy and Send leave out the $ prompts" })
          : null,
        el("span", { className: "cmd-seq", textContent: `#${command.seq}` }),
      ),
      el("pre", { className: "cmd-code", tabIndex: 0 }, el("code", { textContent: command.text })),
      el(
        "div",
        { className: "cmd-actions" },
        el(
          "button",
          { type: "button", className: "slab cmd-copy", onclick: () => copyCommand(command) },
          icon("copy"),
          "Copy",
        ),
        el(
          "button",
          {
            type: "button",
            className: "slab ghost",
            disabled: !target,
            title: target ? `Paste into ${target.name} without pressing Enter` : "No running shell in this workspace",
            onclick: () => sendTo(command, false, source),
          },
          icon("send"),
          "Send",
        ),
        el(
          "button",
          {
            type: "button",
            className: "slab ghost",
            disabled: !target,
            title: target ? `Paste into ${target.name} and press Enter` : "No running shell in this workspace",
            onclick: () => sendTo(command, true, source),
          },
          "Send + Run",
        ),
      ),
    );
  }

  function renderDrawer() {
    const drawer = $("#commands");
    drawer.hidden = !ui.open;
    const source = currentSource();
    const count = $("#cmd-count");
    count.textContent = ui.unseen ? String(ui.unseen) : "";
    count.hidden = !ui.unseen;
    $("#cmd-toggle").setAttribute("aria-pressed", String(ui.open));
    if (!ui.open) return;

    const pool = sources();
    const sourcePick = $("#cmd-source");
    sourcePick.replaceChildren(
      ...(pool.length
        ? pool.map((s) => el("option", { value: s.id, textContent: s.name, selected: source && s.id === source.id }))
        : [el("option", { value: "", textContent: "no agent sessions" })]),
    );
    sourcePick.disabled = pool.length < 2 || isAgent(attachedSession());
    const target = currentTarget(source);
    const targetPool = targets(source);
    const targetPick = $("#cmd-target");
    targetPick.replaceChildren(
      ...(targetPool.length
        ? targetPool.map((s) => el("option", { value: s.id, textContent: s.name, selected: target && s.id === target.id }))
        : [el("option", { value: "", textContent: "no running shell" })]),
    );
    targetPick.disabled = targetPool.length < 2;
    $("#cmd-all").checked = ui.all;

    const list = $("#cmd-list");
    const items = shown(source);
    let empty = null;
    if (!source) {
      empty = "Commands appear here when a Claude Code or Codex session in this workspace suggests a code block.";
    } else if (ui.loading === source.id && !cache[source.id]) {
      empty = "Reading the transcript…";
    } else if (ui.error) {
      empty = `Could not read commands: ${ui.error}`;
    } else if (!items.length) {
      empty = ui.all
        ? `${source.name} has written no code blocks yet.`
        : `${source.name} has suggested no shell commands yet. Ask it for one, or tick "all blocks".`;
    }
    list.replaceChildren(
      ...(empty
        ? [el("li", { className: "cmd-empty", textContent: empty })]
        : items.map((c) => renderItem(c, source, target))),
    );
  }

  function setOpen(open) {
    ui.open = open;
    if (open) {
      ui.unseen = 0;
      const source = currentSource();
      if (source) load(source.id);
    }
    renderDrawer();
    if (open) {
      const first = $("#cmd-list .cmd-copy");
      if (first) first.focus({ preventScroll: true });
    } else {
      focusTerminal();
    }
  }

  const toggle = () => setOpen(!ui.open);

  $("#cmd-source").addEventListener("change", (event) => {
    ui.source = event.target.value || null;
    if (ui.source) load(ui.source);
    renderDrawer();
  });
  $("#cmd-target").addEventListener("change", (event) => {
    ui.target = event.target.value || null;
    renderDrawer();
  });
  $("#cmd-all").addEventListener("change", (event) => {
    ui.all = event.target.checked;
    renderDrawer();
  });
  $("#cmd-close").addEventListener("click", () => setOpen(false));
  $("#commands").addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      setOpen(false);
    }
  });

  // --- pushes ------------------------------------------------------------

  function onEvent(message) {
    const id = message.session_id;
    const added = remember(id, message.commands || []);
    const session = sessionById(id);
    const source = currentSource();
    if (ui.open) renderDrawer();
    const shell = added.filter((c) => c.shell);
    if (!message.fresh || !shell.length || !session) return;
    // Toast for agents in the workspace in view (any, with none attached).
    const attached = attachedSession();
    if (attached && attached.workspace_id !== session.workspace_id) return;
    const showing = ui.open && source && source.id === id;
    if (showing) {
      const node = $(`#cmd-list .cmd[data-seq="${shell[0].seq}"]`);
      if (node) node.classList.add("arrived");
      return;
    }
    if (!ui.open) {
      ui.unseen += shell.length;
      renderDrawer();
    }
    const newest = shell[0];
    toast(
      el(
        "span",
        {},
        el("b", { textContent: session.name }),
        shell.length > 1 ? ` suggested ${shell.length} commands` : " suggested a command",
        el("code", { className: "toast-code", textContent: preview(newest.command) }),
      ),
      {
        actions: [
          el("button", { type: "button", className: "text-btn", textContent: "copy", onclick: () => copyCommand(newest, session.name) }),
          el("button", {
            type: "button",
            className: "text-btn",
            textContent: "view",
            onclick: () => {
              ui.source = id;
              setOpen(true);
            },
          }),
        ],
        timeout: 7000,
      },
    );
  }

  function paletteItems(cmd) {
    const source = currentSource();
    cmd(ui.open ? "Hide commands" : "Show commands", `${ctx.MOD} C`, toggle, "copy");
    if (source) {
      cmd(`Copy last command from ${source.name}`, `${ctx.MOD} Shift C`, copyLast, "copy");
    }
  }

  // The drawer follows the attached session.
  function render() {
    renderDrawer();
  }

  // Keep the cache warm for the source, so Copy last works on first press.
  function prefetch() {
    const source = currentSource();
    if (source && !cache[source.id] && ui.loading !== source.id) load(source.id);
  }

  return { onEvent, toggle, copyLast, paletteItems, render, prefetch, copyText, toast, isOpen: () => ui.open };
};

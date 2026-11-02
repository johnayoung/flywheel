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
    selected: null, // seq of the card the keyboard acts on
    focusPending: false, // opened: focus the selected card once it renders
  };

  const isAgent = (s) => s && (s.kind === "claude" || s.kind === "codex");
  // A shell: a shell session, or an agent restored as a fallback shell.
  const runsShell = (s) => s.kind === "shell" || Array.isArray(s.fallback_command);
  const isRunning = (s) => s && s.live && s.live.pid && s.state === "running";
  const attachedSession = () => sessionById(state.attached);
  const basename = (path) => String(path || "").split("/").filter(Boolean).pop() || String(path || "");

  // A session as the pickers name it: its name, plus the folder it runs in
  // when its workspace has more than one.
  function sessionLabel(s) {
    const workspace = workspaceById(s.workspace_id);
    if (!workspace || !workspace.folders || workspace.folders.length < 2) return s.name;
    const folder = workspace.folders.find((f) => f.path === s.cwd);
    return `${s.name} \u00b7 ${folder ? folder.name || basename(folder.path) : basename(s.cwd)}`;
  }

  // Drop what refers to sessions that no longer exist (closed here or
  // elsewhere, or their workspace deleted): picks, cache, keyboard focus.
  function forgetRemoved() {
    const live = new Set(state.sessions.map((s) => s.id));
    if (ui.source && !live.has(ui.source)) {
      ui.source = null;
      ui.selected = null;
    }
    if (ui.target && !live.has(ui.target)) ui.target = null;
    for (const id of Object.keys(cache)) {
      if (!live.has(id) && ui.loading !== id) delete cache[id];
    }
  }

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

  // Running shells of the source's workspace.
  function targets(source) {
    const workspace = source ? source.workspace_id : state.attached && attachedSession().workspace_id;
    return state.sessions.filter((s) => runsShell(s) && isRunning(s) && s.workspace_id === workspace);
  }

  // The user's pick; else the attached shell; else a shell in the
  // source's folder (where its commands were meant to run); else any.
  function currentTarget(source) {
    const pool = targets(source);
    const attached = attachedSession();
    return (
      pool.find((s) => s.id === ui.target) ||
      (attached && pool.find((s) => s.id === attached.id)) ||
      (source && pool.find((s) => s.cwd === source.cwd)) ||
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
      {
        className: `cmd${command.shell ? " shell" : ""}${command.seq === ui.selected ? " selected" : ""}`,
        dataset: { seq: String(command.seq) },
        tabIndex: command.seq === ui.selected ? 0 : -1,
      },
      el(
        "div",
        { className: "cmd-meta" },
        el("span", { className: "cmd-lang", textContent: lang }),
        el("span", { className: "cmd-when", textContent: when(command.timestamp), title: command.timestamp || "" }),
        el("span", { className: "cmd-lines", textContent: `${lines} line${lines === 1 ? "" : "s"}` }),
        command.command !== command.text
          ? command.command.split("\n").length < lines
            ? el("span", { className: "cmd-note", textContent: "commands only", title: "Copy and Send take the commands without $ prompts, and leave the output lines out" })
            : el("span", { className: "cmd-note", textContent: "prompts stripped", title: "Copy and Send leave out the $ prompts" })
          : null,
        el("span", { className: "cmd-seq", textContent: `#${command.seq}` }),
      ),
      el("pre", { className: "cmd-code", tabIndex: -1 }, el("code", { textContent: command.text })),
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
        ? pool.map((s) => el("option", { value: s.id, textContent: sessionLabel(s), selected: source && s.id === source.id }))
        : [el("option", { value: "", textContent: "no agent sessions" })]),
    );
    sourcePick.disabled = pool.length < 2 || isAgent(attachedSession());
    const target = currentTarget(source);
    const targetPool = targets(source);
    const targetPick = $("#cmd-target");
    targetPick.replaceChildren(
      ...(targetPool.length
        ? targetPool.map((s) => el("option", { value: s.id, textContent: sessionLabel(s), selected: target && s.id === target.id }))
        : [el("option", { value: "", textContent: "no running shell" })]),
    );
    targetPick.disabled = targetPool.length < 2;
    $("#cmd-all").checked = ui.all;

    const list = $("#cmd-list");
    const items = shown(source);
    if (!items.some((c) => c.seq === ui.selected)) ui.selected = items.length ? items[0].seq : null;
    // Re-rendering replaces the cards: note where focus was, to put it
    // back (a lost focus lands on <body>, where no key reaches the drawer).
    const active = document.activeElement;
    const focusedCard = list.contains(active) ? active.closest(".cmd") : null;
    const focusedAction = focusedCard && active.matches("button") ? [...focusedCard.querySelectorAll("button")].indexOf(active) : -1;
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
    markOverflow();
    if (focusedCard || (ui.focusPending && drawer.contains(document.activeElement))) {
      const card = selectedCard();
      const button = card && focusedAction >= 0 ? card.querySelectorAll("button")[focusedAction] : null;
      const into = button || card;
      if (into) {
        into.focus({ preventScroll: true });
        ui.focusPending = false;
      } else if (!drawer.contains(document.activeElement)) {
        drawer.focus({ preventScroll: true });
      }
    }
  }

  // Code wider than its card scrolls sideways; mark those so the edge
  // fades and a hint shows (a cut-off pipeline must not look complete).
  function markOverflow() {
    for (const pre of document.querySelectorAll("#cmd-list .cmd-code")) {
      const update = () => {
        const more = pre.scrollWidth - pre.clientWidth - pre.scrollLeft > 2;
        pre.parentElement.classList.toggle("overflows", more);
      };
      update();
      if (!pre.dataset.watched) {
        pre.dataset.watched = "1";
        pre.addEventListener("scroll", update, { passive: true });
      }
    }
  }

  function selectedCard() {
    return ui.selected === null ? null : $(`#cmd-list .cmd[data-seq="${ui.selected}"]`);
  }

  function selectedCommand() {
    const source = currentSource();
    return shown(source).find((c) => c.seq === ui.selected) || null;
  }

  // Make card `seq` the keyboard's (roving tabindex); `focus` moves focus.
  function select(seq, focus = true) {
    ui.selected = seq;
    for (const card of document.querySelectorAll("#cmd-list .cmd")) {
      const on = card.dataset.seq === String(seq);
      card.classList.toggle("selected", on);
      card.tabIndex = on ? 0 : -1;
    }
    const card = selectedCard();
    if (card && focus) {
      card.focus({ preventScroll: true });
      card.scrollIntoView({ block: "nearest" });
    }
  }

  function moveSelection(by) {
    const items = shown(currentSource());
    if (!items.length) return;
    const at = items.findIndex((c) => c.seq === ui.selected);
    const next = Math.max(0, Math.min(items.length - 1, (at < 0 ? 0 : at) + by));
    select(items[next].seq);
  }

  function setOpen(open) {
    const wasOpen = ui.open;
    ui.open = open;
    if (open) {
      ui.unseen = 0;
      // Keys go to the drawer at once (not the terminal behind it), and
      // to the selected card once the list has loaded.
      ui.focusPending = true;
      if (!wasOpen) ui.selected = null;
      renderDrawer();
      const drawer = $("#commands");
      const card = selectedCard();
      (card || drawer).focus({ preventScroll: true });
      if (card) ui.focusPending = false;
      const source = currentSource();
      if (source) load(source.id);
    } else {
      ui.focusPending = false;
      renderDrawer();
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
  // Tabbing or clicking into a card makes it the keyboard's.
  $("#cmd-list").addEventListener("focusin", (event) => {
    const card = event.target.closest(".cmd[data-seq]");
    if (card && card.dataset.seq !== String(ui.selected)) select(Number(card.dataset.seq), false);
  });
  // Keys inside the drawer: Esc closes; arrows / j k / Home End pick a
  // card; Enter or c copies it; 1-9 copy the nth; s sends it, Shift S
  // sends and runs. Form fields and buttons keep their own keys.
  $("#commands").addEventListener("keydown", (event) => {
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const done = () => {
      event.preventDefault();
      event.stopPropagation();
    };
    if (event.key === "Escape") {
      done();
      setOpen(false);
      return;
    }
    const field = event.target.closest("select, input, textarea");
    if (field) return;
    const onButton = !!event.target.closest("button");
    const command = selectedCommand();
    const source = currentSource();
    if (event.key === "ArrowDown" || event.key === "j") {
      done();
      moveSelection(1);
    } else if (event.key === "ArrowUp" || event.key === "k") {
      done();
      moveSelection(-1);
    } else if (event.key === "Home") {
      done();
      moveSelection(-Infinity);
    } else if (event.key === "End") {
      done();
      moveSelection(Infinity);
    } else if ((event.key === "Enter" && !onButton) || event.key === "c") {
      if (!command) return;
      done();
      copyCommand(command);
    } else if (/^[1-9]$/.test(event.key)) {
      const nth = shown(source)[Number(event.key) - 1];
      if (!nth) return;
      done();
      select(nth.seq);
      copyCommand(nth);
    } else if (event.key === "s" || event.key === "S") {
      if (!command) return;
      done();
      sendTo(command, event.key === "S", source);
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
    cmd(ui.open ? "Hide suggested commands" : "Show suggested commands", `${ctx.MOD} C`, toggle, "copy");
    if (source) {
      cmd(`Copy last command from ${source.name}`, `${ctx.MOD} Shift C`, copyLast, "copy");
    }
  }

  // The drawer follows the attached session.
  function render() {
    forgetRemoved();
    renderDrawer();
  }

  // Keep the cache warm for the source, so Copy last works on first press.
  function prefetch() {
    const source = currentSource();
    if (source && !cache[source.id] && ui.loading !== source.id) load(source.id);
  }

  return { onEvent, toggle, copyLast, paletteItems, render, prefetch, copyText, toast, isOpen: () => ui.open };
};

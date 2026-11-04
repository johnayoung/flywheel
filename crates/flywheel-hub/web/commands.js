// flywheel hub: the Commands drawer. Code blocks an agent suggested, read
// by the hub from the agent's transcript (exact text, no terminal margins
// or wrapping), with Copy, Send (paste into a shell session, never Enter)
// and Send + Run. With no shell running where the agent works, "Open shell
// & send" starts the agent's companion shell (same directory) and pastes
// once its prompt is up. app-base.js creates it with its helpers; protocol:
// GET /api/sessions/{id}/commands, POST /api/sessions/{id}/paste,
// POST /api/sessions/{id}/companion, and the events socket's
// {"type":"commands"} pushes.
"use strict";

window.FwHubCommands = function (ctx) {
  const { $, el, icon, api, state, sessionById, workspaceById, focusTerminal } = ctx;

  const MAX_CACHED = 500;
  // The "send to" pick that opens the agent's companion shell.
  const NEW_COMPANION = "companion:new";
  const cache = {}; // session id -> commands, newest first
  const ui = {
    open: false,
    source: null, // session id whose commands are shown (chosen by the user)
    target: null, // session id Send pastes into (chosen by the user)
    all: false, // every block, not only shell commands
    loading: null, // session id being fetched
    error: "",
    unseen: {}, // agent session id -> shell commands not yet seen in the drawer
    selected: null, // seq of the card the keyboard acts on
    wanted: null, // seq to select and show once the list has it (a search hit)
    focusPending: false, // opened: focus the selected card once it renders
  };

  const { isAgent } = ctx;
  // Shortcut hints follow the keymap ("" when unbound).
  const withKey = (text, id) => window.FwHubFmt.withKey(text, ctx.keyOf(id));
  const flipKey = () => ctx.keyOf("companion.flip");
  // A shell: a shell session, or an agent restored as a fallback shell.
  const runsShell = (s) => s.kind === "shell" || Array.isArray(s.fallback_command);
  const isRunning = (s) => s && s.live && s.live.pid && s.state === "running";
  const attachedSession = () => sessionById(state.attached);
  const basename = (path) => String(path || "").split("/").filter(Boolean).pop() || String(path || "");

  // A session as the pickers name it: its name, plus the folder it runs in
  // when its workspace has more than one, and its worktree's branch.
  function sessionLabel(s) {
    const workspace = workspaceById(s.workspace_id);
    const branch = s.worktree ? ` \u00b7 wt ${s.worktree.branch}` : "";
    if (!workspace || !workspace.folders || workspace.folders.length < 2) return `${s.name}${branch}`;
    const home = s.worktree ? s.worktree.repo : s.cwd;
    const folder = workspace.folders.find((f) => f.path === home);
    return `${s.name} \u00b7 ${folder ? folder.name || basename(folder.path) : basename(home)}${branch}`;
  }

  // An agent's running companion shell.
  const companionOf = (agent) =>
    agent ? state.sessions.find((s) => s.companion_of === agent.id && isRunning(s)) || null : null;

  // Where a session works, for hints: its worktree branch or its folder.
  // (by its label, as the sidebar names it).
  function placeOf(s) {
    if (s.worktree) return `worktree ${s.worktree.branch}`;
    const workspace = workspaceById(s.workspace_id);
    const folder = workspace && workspace.folders && workspace.folders.find((f) => f.path === s.cwd);
    return folder && folder.name ? folder.name : basename(s.cwd);
  }

  // Drop what refers to sessions that no longer exist (closed here or
  // elsewhere, or their workspace deleted): picks, cache, keyboard focus.
  function forgetRemoved() {
    const live = new Set(state.sessions.map((s) => s.id));
    if (ui.source && !live.has(ui.source)) {
      ui.source = null;
      ui.selected = null;
    }
    if (ui.target && ui.target !== NEW_COMPANION && !live.has(ui.target)) ui.target = null;
    for (const id of Object.keys(cache)) {
      if (!live.has(id) && ui.loading !== id) delete cache[id];
    }
    for (const id of Object.keys(ui.unseen)) {
      if (!live.has(id)) delete ui.unseen[id];
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

  // One toast layer for the whole page, newest last, at most three. It is
  // a manual popover, so it shows in the top layer, over any dialog. A
  // modal dialog makes everything outside it inert (a toast's buttons
  // would not take clicks), so while one is open the layer moves into the
  // topmost and is shown again there; it returns to the page when the
  // dialogs close. An open dialog or drawer never sits under the toasts
  // either: while both show, a dialog ends above the stack and a drawer
  // (Commands, Flywheel, Review, Queue; a full-screen sheet on phones)
  // keeps its frame but lifts its content and footer above it
  // (`--toast-room`, read by the rules in app-dialogs.css).
  const toastRoot = $("#toasts");
  const modals = []; // open modal dialogs, the most recently opened last

  const hostWatch = new MutationObserver(() => {
    if (!toastRoot.isConnected) raiseToasts();
  });

  const isModal = (dialog) => {
    try {
      return dialog.matches(":modal");
    } catch (_) {
      return true;
    }
  };

  function raiseToasts() {
    for (let i = modals.length - 1; i >= 0; i--) {
      if (!modals[i].isConnected || !modals[i].open) modals.splice(i, 1);
    }
    const host = modals[modals.length - 1] || document.body;
    const showing = typeof toastRoot.showPopover === "function";
    if (toastRoot.parentElement !== host) {
      host.append(toastRoot);
      // A dialog that redraws itself (replaceChildren) drops the layer:
      // it is put back.
      hostWatch.disconnect();
      if (host !== document.body) hostWatch.observe(host, { childList: true });
    }
    if (!toastRoot.children.length || !showing) return;
    try {
      if (toastRoot.matches(":popover-open")) toastRoot.hidePopover();
      toastRoot.showPopover();
    } catch (_) {
      // No popover support: toasts stay in the page.
    }
  }

  // The drawers toasts make room in.
  const drawers = [...document.querySelectorAll("aside.commands")];

  // An open dialog ends above the toast stack only when the stack is in its
  // way: a toast off to the side of a centred dialog leaves it where it is
  // (it would jump as each toast came and went). Docking moves a dialog
  // vertically only, so the side-by-side test is stable either way. A
  // drawer in the stack's way pads its bottom by the stack's height: its
  // box does not move, so the test stays stable for it too.
  function dockToasts() {
    let box = null;
    for (const t of toastRoot.children) {
      const r = t.getBoundingClientRect();
      if (!r.width || !r.height) continue;
      box = box
        ? { left: Math.min(box.left, r.left), right: Math.max(box.right, r.right), top: Math.min(box.top, r.top) }
        : { left: r.left, right: r.right, top: r.top };
    }
    const inWay =
      box &&
      modals.some((dialog) => {
        if (!dialog.isConnected || !dialog.open) return false;
        const d = dialog.getBoundingClientRect();
        return box.left < d.right && box.right > d.left;
      });
    const room = inWay ? Math.ceil(window.innerHeight - box.top + 12) : 0;
    document.documentElement.style.setProperty("--toast-room", `${room}px`);
    document.body.classList.toggle("toast-docked", room > 0);
    for (const drawer of drawers) {
      const d = drawer.hidden ? null : drawer.getBoundingClientRect();
      const under = box && d && d.width && box.left < d.right && box.right > d.left && box.top < d.bottom;
      const pad = under ? Math.ceil(d.bottom - box.top + 8) : 0;
      drawer.style.setProperty("--toast-room", `${pad}px`);
      drawer.classList.toggle("toast-docked", pad > 0);
    }
  }

  new MutationObserver((changes) => {
    let moved = false;
    for (const change of changes) {
      const dialog = change.target;
      if (dialog.tagName !== "DIALOG") continue;
      const at = modals.indexOf(dialog);
      if (at >= 0) modals.splice(at, 1);
      if (dialog.open && isModal(dialog)) modals.push(dialog);
      moved = true;
    }
    if (moved) raiseToasts();
    dockToasts();
  }).observe(document.body, { subtree: true, attributes: true, attributeFilter: ["open"] });
  if (typeof ResizeObserver === "function") new ResizeObserver(dockToasts).observe(toastRoot);
  new MutationObserver(dockToasts).observe(toastRoot, { childList: true });
  for (const drawer of drawers) new MutationObserver(dockToasts).observe(drawer, { attributes: true, attributeFilter: ["hidden"] });
  window.addEventListener("resize", dockToasts);

  function toast(message, { actions = [], timeout = 4500, tone = "" } = {}) {
    const root = toastRoot;
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
      if (event.target.closest("button, a")) setTimeout(dismiss, 0);
    });
    root.append(node);
    while (root.children.length > 3) root.firstElementChild.remove();
    raiseToasts();
    dockToasts();
    // A phone's open sessions drawer hides the toasts (they would cover
    // its footer): the clock waits until it closes, so none goes unseen.
    const hiddenNow = () =>
      document.body.classList.contains("side-open") && window.matchMedia("(max-width: 760px)").matches;
    const later = () => (hiddenNow() ? setTimeout(later, 1000) : setTimeout(dismiss, timeout));
    if (timeout) later();
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

  // The attached agent session; else the user's pick; else (a companion
  // shell) its own agent; else the agent working in the attached shell's
  // directory that suggested something most recently; else the one in this
  // workspace that did. A shell's default never comes from an agent in
  // another directory (another worktree) while one works here.
  function currentSource() {
    const attached = attachedSession();
    if (isAgent(attached)) return attached;
    const pool = sources();
    const picked = pool.find((s) => s.id === ui.source);
    if (picked) return picked;
    const own = attached && attached.companion_of ? pool.find((s) => s.id === attached.companion_of) : null;
    if (own) return own;
    const newest = (list) => list.slice().sort((a, b) => (newestSeqTime(b.id) > newestSeqTime(a.id) ? 1 : -1))[0] || null;
    const here = attached ? pool.filter((s) => s.cwd === attached.cwd) : [];
    return newest(here) || newest(pool);
  }

  // Running shells of the source's workspace.
  function targets(source) {
    const workspace = source ? source.workspace_id : state.attached && attachedSession().workspace_id;
    return state.sessions.filter((s) => runsShell(s) && isRunning(s) && s.workspace_id === workspace);
  }

  // Whether Send can open the source agent's companion shell.
  const canOpenCompanion = (source) => isAgent(source) && !companionOf(source);

  // Where Send goes: {session} (a running shell) or {open: agent} (start
  // the agent's companion shell first), or null. The user's pick; else the
  // agent's companion; else the attached shell; else a shell in the
  // agent's directory (where its commands were meant to run); else a new
  // companion there; else any shell of the workspace.
  function currentTarget(source) {
    const pool = targets(source);
    const attached = attachedSession();
    if (ui.target === NEW_COMPANION && canOpenCompanion(source)) return { open: source };
    const session =
      pool.find((s) => s.id === ui.target) ||
      companionOf(source) ||
      (attached && pool.find((s) => s.id === attached.id)) ||
      (source && pool.find((s) => s.cwd === source.cwd));
    if (session) return { session };
    if (canOpenCompanion(source)) return { open: source };
    return pool[0] ? { session: pool[0] } : null;
  }

  // The line under the pickers saying where Send goes.
  function targetHint(source, target) {
    if (!target) {
      return source
        ? `No shell is running in this workspace to send to; copy the command instead.`
        : "";
    }
    if (target.open) {
      return `No shell runs in ${placeOf(target.open)} yet. Open shell starts ${target.open.name}'s companion shell there, waits for its prompt, then pastes (Run also presses Enter).${flipKey() ? ` ${flipKey()} flips between them.` : ""}`;
    }
    const where = target.session.cwd ? ` in ${placeOf(target.session)}` : "";
    const own = source && target.session.companion_of === source.id ? ` (${source.name}'s companion)` : "";
    const elsewhere = source && target.session.cwd !== source.cwd ? `, not where ${source.name} works (${placeOf(source)})` : "";
    return `Send pastes into ${target.session.name}${own}${where}${elsewhere} without pressing Enter; Send + Run also presses it.`;
  }

  function shown(source) {
    const list = (source && cache[source.id]) || [];
    return ui.all ? list : list.filter((c) => c.shell);
  }

  // --- actions -----------------------------------------------------------

  async function sendTo(command, run, source) {
    const picked = currentTarget(source);
    if (!picked) {
      toast("No running shell in this workspace to send to.", { tone: "bad" });
      return;
    }
    if (picked.open) {
      await openAndSend(command, run, picked.open);
      return;
    }
    const target = picked.session;
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

  // Start (or reuse) `agent`'s companion shell and paste into it once its
  // prompt is up.
  async function openAndSend(command, run, agent) {
    const busy = toast(`Opening a shell in ${placeOf(agent)}…`, { timeout: 0 });
    try {
      const sent = await api("POST", `/api/sessions/${agent.id}/companion`, {
        text: command.command,
        run,
        ...ctx.termSize(),
      });
      busy.remove();
      if (ui.target === NEW_COMPANION) ui.target = null;
      await ctx.refresh();
      const shell = sent.session;
      const late = sent.ready === "timeout" ? " Its prompt was slow to appear; check it." : "";
      toast(`${sent.created ? "Opened" : "Used"} ${shell.name} and ${run ? "ran it" : "pasted it (not run: press Enter there)"}.${late}`, {
        actions: [el("button", { type: "button", className: "text-btn", textContent: "open", onclick: () => ctx.openSession(shell.id) })],
        timeout: 4000,
        tone: late ? "bad" : "",
      });
    } catch (error) {
      busy.remove();
      toast(error.message, { tone: "bad", timeout: 6000 });
    }
  }

  // Alt+`: from an agent to its companion shell (started on first use),
  // and back.
  async function flipCompanion() {
    const attached = attachedSession();
    if (!attached) return;
    if (attached.companion_of) {
      if (sessionById(attached.companion_of)) ctx.flipTo(attached.companion_of, attached.id);
      else toast(`${attached.name}'s agent session is gone.`, { timeout: 2500 });
      return;
    }
    if (!isAgent(attached)) {
      toast(`${flipKey() || "The companion flip"} flips between an agent session and its companion shell; ${attached.name} is a shell.`, { timeout: 3000 });
      return;
    }
    const running = companionOf(attached);
    if (running) {
      ctx.flipTo(running.id, attached.id);
      return;
    }
    try {
      const shell = await api("POST", `/api/sessions/${attached.id}/companion`, ctx.termSize());
      await ctx.refresh();
      ctx.flipTo(shell.id, attached.id);
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

  function sendButtons(command, source, target) {
    const opens = !!(target && target.open);
    const name = target && target.session ? target.session.name : "";
    const label = opens ? ["Open shell & send", "Open shell & run"] : ["Send", "Send + Run"];
    const title = !target
      ? "No running shell in this workspace, and no agent to open one for"
      : opens
        ? [`Open ${target.open.name}'s companion shell in ${placeOf(target.open)} and paste, without Enter`, "Open the companion shell, paste, and press Enter"]
        : [`Paste into ${name} without pressing Enter`, `Paste into ${name} and press Enter`];
    return [false, true].map((run, i) =>
      el(
        "button",
        {
          type: "button",
          className: `slab ghost${opens ? " opens" : ""}`,
          disabled: !target,
          title: Array.isArray(title) ? title[i] : title,
          onclick: () => sendTo(command, run, source),
        },
        i === 0 ? icon("send") : null,
        label[i],
      ));
  }

  function renderItem(command, source, target, position) {
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
        // Its place in the list (the 1-9 key that copies it); the block's
        // id in the transcript is the tooltip.
        el("span", { className: "cmd-seq", textContent: position <= 9 ? `${position}` : "", title: `block #${command.seq}${position <= 9 ? `: key ${position} copies it` : ""}` }),
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
        ...sendButtons(command, source, target),
        ctx.handoff && source
          ? el("button", {
            type: "button",
            className: "slab ghost cmd-handoff",
            title: "Send this block to another agent session, under where it is from (editable first)",
            onclick: () => ctx.handoff().sendCommand(source.id, command.seq),
          }, "To agent…")
          : null,
      ),
    );
  }

  function renderDrawer() {
    const drawer = $("#commands");
    drawer.hidden = !ui.open;
    const source = currentSource();
    // What the drawer shows is seen.
    if (ui.open && source) delete ui.unseen[source.id];
    // The badge counts the drawer's source only: the attached agent's own
    // blocks, or (from a shell) the workspace agent that suggested last,
    // named so its count is not taken for this session's.
    const unseen = (source && ui.unseen[source.id]) || 0;
    const attached = attachedSession();
    const count = $("#cmd-count");
    count.replaceChildren(...(unseen
      ? [String(unseen), source.id !== (attached && attached.id)
        ? el("span", { className: "cmd-from", textContent: ` \u00b7 ${source.name}` })
        : ""]
      : []));
    count.hidden = !unseen;
    $("#cmd-toggle").title = unseen
      ? withKey(`Suggested commands: ${unseen} new from ${source.name}`, "commands.toggle")
      : withKey("Suggested commands", "commands.toggle");
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
    const options = targetPool.map((s) =>
      el("option", {
        value: s.id,
        textContent: s.companion_of && source && s.companion_of === source.id ? `${sessionLabel(s)} (companion)` : sessionLabel(s),
        selected: !!(target && target.session && s.id === target.session.id),
      }));
    if (canOpenCompanion(source)) {
      options.push(el("option", {
        value: NEW_COMPANION,
        textContent: `new shell in ${placeOf(source)} (companion)`,
        selected: !!(target && target.open),
      }));
    }
    targetPick.replaceChildren(...(options.length ? options : [el("option", { value: "", textContent: "no running shell" })]));
    targetPick.disabled = options.length < 2;
    $("#cmd-hint").textContent = targetHint(source, target);
    $("#cmd-all").checked = ui.all;

    const list = $("#cmd-list");
    const items = shown(source);
    let reveal = false;
    if (ui.wanted !== null && items.some((c) => c.seq === ui.wanted)) {
      ui.selected = ui.wanted;
      ui.wanted = null;
      reveal = true;
    }
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
        : items.map((c, i) => renderItem(c, source, target, i + 1))),
    );
    markOverflow();
    if (reveal) {
      const card = selectedCard();
      if (card) {
        card.scrollIntoView({ block: "center" });
        card.classList.add("arrived");
        card.focus({ preventScroll: true });
        ui.focusPending = false;
      }
    }
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
    if (open && !wasOpen && ctx.onOpen) ctx.onOpen();
    ui.open = open;
    if (open) {
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

  // Open the drawer on session `id`'s block `seq` (a search hit; `all`:
  // it is not a shell block, so every block is listed).
  function openAt(id, seq, all) {
    ui.source = id;
    if (all) ui.all = true;
    ui.wanted = seq;
    if (ui.open) {
      const source = currentSource();
      if (source) load(source.id);
    } else {
      setOpen(true);
    }
  }

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
    ui.unseen[id] = (ui.unseen[id] || 0) + shell.length;
    renderDrawer();
    // Settings: notifications, "Command toasts".
    if (ctx.toastsOn && !ctx.toastsOn()) return;
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
    cmd(ui.open ? "Hide suggested commands" : "Show suggested commands", ctx.keyOf("commands.toggle"), toggle, "copy");
    if (source) {
      cmd(`Copy last command from ${source.name}`, ctx.keyOf("commands.copy_last"), copyLast, "copy");
    }
    const attached = attachedSession();
    if (attached && attached.companion_of && sessionById(attached.companion_of)) {
      cmd(`Back to agent ${sessionById(attached.companion_of).name}`, flipKey(), flipCompanion, "claude");
    } else if (isAgent(attached)) {
      const running = companionOf(attached);
      cmd(running ? `Companion shell of ${attached.name}` : `Open a companion shell for ${attached.name}`, flipKey(), flipCompanion, "shell");
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

  return { onEvent, toggle, openAt, copyLast, flipCompanion, paletteItems, render, prefetch, copyText, toast, isOpen: () => ui.open };
};

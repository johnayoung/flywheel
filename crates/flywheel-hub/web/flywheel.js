// flywheel hub: the Flywheel panel. For every workspace folder that is a
// flywheel-adopted repo: its active phases and tasks with lifecycle
// status, in-flight and recent runs with grader receipts, worker and
// autopilot liveness, claims, and the stop / strand alerts -- read-only,
// beside the terminals. Actions: start `fw worker` in a hub shell session
// there (the whole queue, or one task: `fw worker --task ID`, which runs
// only it and exits once it settles), open a run's output (its `fw show` summary and redacted
// agent transcript), and drive autopilot (autopilot.js). app-base.js creates it with its helpers; protocol:
// GET /api/flywheel, /api/flywheel/panel, /api/flywheel/run,
// /api/claude-accounts (the start-worker account picker, launcher.js's
// FwHubAccounts), POST /api/flywheel/worker, /api/flywheel/pauses/clear
// (the panel's active session-limit pauses), and the events socket's {"type":"flywheel"} and
// {"type":"flywheel_reset"} pushes. Another machine's folders
// (machines.js) are read through this hub's proxy; their keys here are
// `<machine>\u0001<path>` (a path alone is this hub's), so the same repo
// path on two machines stays two panels.
"use strict";

window.FwHubFlywheel = function (ctx) {
  const { $, el, icon, api, state, store, workspaceById, folderLabel, openSession, toast, confirmDialog } = ctx;

  const RUN_POLL_MS = 2000;
  // Transcript entries the run viewer keeps (the oldest are dropped).
  const RUN_WINDOW = 3000;
  // The monitor re-reads every 15s but pushes only changes: past this,
  // the open panel re-fetches its (server-cached) read to stay honest.
  const PANEL_STALE_MS = 20000;
  // Needs you lists a folder's alerts one by one up to this many, then as
  // one row that opens its panel.
  const ATTENTION_PER_FOLDER = 2;
  const SEEN_KEY = "fw-hub.flywheel-seen";
  // The drawer's sections collapsed by title (this browser), kept across
  // closing and reopening it.
  const COLLAPSED_KEY = "flywheel-collapsed";
  const collapsed = new Set(store.get(COLLAPSED_KEY, []));
  const ui = {
    enabled: false,
    loaded: false, // this hub's folders were read once
    summaries: new Map(), // folder path -> summary
    folder: null, // folder whose panel is open
    workspaceId: null, // workspace it was opened from
    panel: null,
    loading: false,
    error: "",
    seq: 0,
    run: null, // the run viewer's state
    pillSession: undefined, // the attached session the pill last showed
    accounts: null, // {folder, workspaceId, list, pick}: the start-worker account picker
    pauseReasons: new Map(), // pause id -> the clear reason being typed
  };

  const LIGHTS = ["failed", "parked", "awaiting", "blocked", "running", "pending", "verified"];
  const KIND_LABEL = {
    failed: "failed",
    stranded: "stranded",
    stopped: "stopped",
    awaiting: "needs you",
    blocked: "blocked",
  };

  // --- folders on machines -----------------------------------------------

  const SEP = "\u0001";
  const keyOf = (machine, path) => (machine ? `${machine}${SEP}${path}` : path);
  const machineOfKey = (key) => (key && key.includes(SEP) ? key.slice(0, key.indexOf(SEP)) : null);
  const pathOf = (key) => (key && key.includes(SEP) ? key.slice(key.indexOf(SEP) + 1) : key);
  const via = (key) => ctx.machinePrefix(machineOfKey(key));
  const sameMachine = (item, key) => (item.machine || null) === machineOfKey(key);
  // A summary or panel as kept here: its folder is the key.
  const keyed = (machine, value) => (machine ? { ...value, folder: keyOf(machine, value.folder) } : value);

  // --- time --------------------------------------------------------------

  const { age: ago, span } = ctx;
  function clock(iso) {
    const date = new Date(iso);
    return Number.isNaN(date.getTime()) ? "" : date.toLocaleTimeString([], { hour12: false });
  }
  const { tokens, dollars } = window.FwHubFmt;

  // --- data --------------------------------------------------------------

  // One machine's folders (`null`: this hub's), replacing what it had.
  async function reloadMachine(machine) {
    try {
      const data = await api("GET", `${ctx.machinePrefix(machine)}/api/flywheel`);
      if (!machine) {
        ui.enabled = !!data.enabled;
        ui.loaded = true;
      }
      for (const key of [...ui.summaries.keys()]) {
        if (machineOfKey(key) === (machine || null)) ui.summaries.delete(key);
      }
      for (const s of data.folders || []) {
        const kept = keyed(machine, s);
        ui.summaries.set(kept.folder, kept);
        prune(kept.folder, kept);
      }
    } catch (_) {
      // Unread: the board stops saying "reading" (its empty state shows).
      if (!machine && !ui.loaded) {
        ui.loaded = true;
        changed();
      }
      return;
    }
    changed();
    if (ui.folder && machineOfKey(ui.folder) === (machine || null)) loadPanel();
  }

  const reload = () => reloadMachine(null);

  function onEvent(message, machine) {
    if (message.type === "flywheel_reset") {
      reloadMachine(machine || null);
      return;
    }
    if (!machine) ui.enabled = true;
    const key = keyOf(machine, message.folder);
    if (message.summary) {
      const kept = keyed(machine, message.summary);
      const before = ui.summaries.get(key);
      ui.summaries.set(key, kept);
      prune(key, kept);
      // Autopilot work newly waiting on a gate: this device hears it
      // (notify.js decides whether to show it).
      if (before && (kept.autopilot_gates || 0) > (before.autopilot_gates || 0) && ctx.notifyGate) {
        ctx.notifyGate(key, labelFor(key), kept.autopilot_gates);
      }
    } else {
      ui.summaries.delete(key);
    }
    changed();
    if (ui.folder === key) {
      if (message.summary) loadPanel();
      else renderDrawer();
    }
  }

  function changed() {
    ctx.rerender();
    renderPill();
    if (ctx.onSummaries) ctx.onSummaries();
  }

  // The panel ignoring when it was read (and in-flight runs' ages).
  function content(panel) {
    return JSON.stringify({
      ...panel,
      generated_at: null,
      live: panel.live.map((r) => ({ ...r, age_seconds: null, idle_seconds: null })),
    });
  }

  // Folder `key`'s panel, read through its machine's hub.
  async function readPanel(key) {
    return keyed(machineOfKey(key), await api("GET", `${via(key)}/api/flywheel/panel?folder=${encodeURIComponent(pathOf(key))}`));
  }

  async function loadPanel() {
    const folder = ui.folder;
    if (!folder) return;
    const seq = ++ui.seq;
    ui.loading = !ui.panel || ui.panel.folder !== folder;
    if (ui.loading) renderDrawer();
    try {
      const panel = await readPanel(folder);
      if (seq !== ui.seq || ui.folder !== folder) return;
      const same = ui.panel && ui.panel.folder === folder && !ui.error && content(ui.panel) === content(panel);
      ui.panel = panel;
      if (same) {
        // A re-read that changed nothing: freshen the stamp, keep the
        // DOM (open folds, scroll) as it is.
        stamp();
        return;
      }
      ui.error = "";
    } catch (error) {
      if (seq !== ui.seq || ui.folder !== folder) return;
      ui.error = error.message;
    }
    ui.loading = false;
    renderDrawer();
  }

  const summary = (folder) => ui.summaries.get(folder) || null;

  // --- seen alerts ----------------------------------------------------------
  // Like a finished agent turn, an alert this browser has acknowledged
  // leaves Needs you (it stays in the panel, dimmed) until it changes or a
  // new one appears. Kept per browser, pruned to the alerts that still
  // exist.

  const alertKey = (a) => `${a.kind}\u0000${a.subject}\u0000${a.detail}`;
  const seen = loadSeen();

  function loadSeen() {
    try {
      const raw = JSON.parse(window.localStorage.getItem(SEEN_KEY) || "{}");
      return new Map(Object.entries(raw).map(([folder, keys]) => [folder, new Set(keys)]));
    } catch (_) {
      return new Map();
    }
  }

  function saveSeen() {
    try {
      const raw = Object.fromEntries([...seen].map(([folder, keys]) => [folder, [...keys]]));
      window.localStorage.setItem(SEEN_KEY, JSON.stringify(raw));
    } catch (_) {
      // Private mode or a full quota: seen marks last for this page only.
    }
  }

  function isSeen(folder, alert) {
    const keys = seen.get(folder);
    return !!keys && keys.has(alertKey(alert));
  }

  function unseen(folder, s) {
    return s ? s.alerts.filter((a) => !isSeen(folder, a)) : [];
  }

  function prune(folder, s) {
    const keys = seen.get(folder);
    if (!keys) return;
    const now = new Set(s.alerts.map(alertKey));
    let dirty = false;
    for (const key of keys) {
      if (!now.has(key)) {
        keys.delete(key);
        dirty = true;
      }
    }
    if (!keys.size) seen.delete(folder);
    if (dirty) saveSeen();
  }

  function markSeen(folder, on) {
    const s = summary(folder);
    if (!s) return;
    if (on) seen.set(folder, new Set(s.alerts.map(alertKey)));
    else seen.delete(folder);
    saveSeen();
    changed();
    renderDrawer();
  }

  // The worst thing about a folder, for its badge.
  function tone(s) {
    if (!s) return "";
    if (unseen(s.folder, s).length) return "alert";
    if (s.live_runs || s.worker_live || s.autopilot_live) return "live";
    return "idle";
  }

  function total(s) {
    return Object.values(s.counts).reduce((a, b) => a + b, 0);
  }

  // The adopted folder a session runs in (its own folder, or above it).
  // A sibling-worktree session (worktree.js) whose worktree is not watched
  // itself falls back to the folder it was cut from, labelled as such
  // (worktreeNote): its worktree has a flywheel store of its own, empty
  // until a worker runs there.
  function folderOf(session) {
    if (!session) return null;
    const cwd = session.cwd || "";
    let best = null;
    for (const key of ui.summaries.keys()) {
      if (!sameMachine(session, key)) continue;
      const folder = pathOf(key);
      if ((cwd === folder || cwd.startsWith(`${folder}/`)) && (!best || key.length > best.length)) best = key;
    }
    const base = session.worktree ? keyOf(session.machine, session.worktree.repo) : null;
    if (!best && base && ui.summaries.has(base)) best = base;
    return best;
  }

  // Whether `session` reaches `folder`'s panel only through its worktree's
  // base folder (not by running in it).
  function viaWorktree(session, key) {
    const folder = pathOf(key);
    if (!session || !sameMachine(session, key) || !session.worktree || session.worktree.repo !== folder) return false;
    const cwd = session.cwd || "";
    return cwd !== folder && !cwd.startsWith(`${folder}/`);
  }

  // What the panel says about checkouts: a folder that is itself a linked
  // worktree reads its own store; a worktree session sees its base's loop.
  function worktreeNote(panel) {
    const session = ctx.sessionById(state.attached);
    if (panel.worktree_of) {
      const main = panel.worktree_of.split("/").filter(Boolean).pop() || panel.worktree_of;
      return `Linked worktree of ${panel.worktree_of}. Flywheel keeps a store per checkout: this panel and Start worker here use this worktree's own, not ${main}'s.`;
    }
    if (viaWorktree(session, panel.folder)) {
      return `${session.name} runs in worktree ${session.worktree.path} (branch ${session.worktree.branch}), a separate checkout with its own flywheel store. Shown: ${labelFor(panel.folder)}'s loop.`;
    }
    return "";
  }

  // Hub sessions running a worker in `folder` (started from this panel;
  // one restored after a hub restart is a plain shell, and one whose
  // worker exited is its shell now).
  function workerSessions(key) {
    const folder = pathOf(key);
    return state.sessions.filter((s) =>
      sameMachine(s, key) && s.cwd === folder && s.live && s.live.pid && s.state === "running" && !s.fallback_command &&
      Array.isArray(s.command) && s.command[3] === "fw-worker" && (!s.worker || s.worker.running));
  }

  function workspaceFor(key) {
    const folder = pathOf(key);
    const here = (w) => sameMachine(w, key) && w.folders.some((f) => f.path === folder);
    const preferred = ui.workspaceId && workspaceById(ui.workspaceId);
    if (preferred && here(preferred)) return preferred;
    return state.workspaces.find(here) || null;
  }

  // A folder's label; another machine's says which.
  function labelFor(key) {
    const folder = pathOf(key);
    const machine = machineOfKey(key);
    let label = folder.split("/").filter(Boolean).pop() || folder;
    for (const w of state.workspaces) {
      if (!sameMachine(w, key)) continue;
      const f = w.folders.find((x) => x.path === folder);
      if (f) {
        label = folderLabel(f);
        break;
      }
    }
    return machine ? `${label} (${machine})` : label;
  }

  // --- sidebar: folder chips --------------------------------------------

  // `node` is the plain folder chip; an adopted folder's becomes a button
  // that opens its panel, with a verified / total count and a tone dot.
  function folderChip(folder, node, workspaceId) {
    const key = keyOf((workspaceById(workspaceId) || {}).machine, folder.path);
    const s = summary(key);
    if (!s) return node;
    const count = total(s);
    const verified = s.counts.verified || 0;
    const t = tone(s);
    const fresh = unseen(key, s).length;
    const why = fresh
      ? `${fresh} need${fresh === 1 ? "s" : ""} you`
      : s.live_runs ? `${s.live_runs} running` : s.worker_live ? "worker live" : "idle";
    const tasks = count ? `${verified}/${count} verified` : "no active tasks";
    const wt = s.worktree_of ? ` Linked worktree of ${s.worktree_of}: its own store.` : "";
    return el(
      "button",
      {
        type: "button",
        className: `fw-chip ${t}${ui.folder === key ? " open" : ""}`,
        title: `${node.title}\nflywheel: ${tasks}, ${why}.${wt} Open the Flywheel panel.`,
        "aria-label": `${folderLabel(folder)}: Flywheel panel (${tasks}, ${why})${s.worktree_of ? ", linked worktree" : ""}`,
        onclick: (event) => {
          event.stopPropagation();
          open(key, workspaceId);
        },
      },
      node,
      el(
        "b",
        { className: "fw-badge" },
        el("i", { className: `fw-dot ${t}`, "aria-hidden": "true" }),
        count ? `${verified}/${count}` : "fw",
        fresh ? el("span", { className: "fw-badge-alerts", textContent: String(fresh), title: why }) : null,
      ),
    );
  }

  // --- attention list ------------------------------------------------------

  const RANK = { failed: 0, stranded: 1, awaiting: 2, blocked: 3, stopped: 4 };
  const byRank = (a, b) => (RANK[a.kind] ?? 9) - (RANK[b.kind] ?? 9);

  // Every adopted folder's unseen alerts, for "Needs you": one row each
  // while a folder has few, else one row for the folder (so a repo with
  // dozens of old strands cannot bury a blocked agent). `count` is how
  // many alerts a row stands for.
  function attention() {
    const out = [];
    for (const [folder, s] of ui.summaries) {
      const label = labelFor(folder);
      const alerts = unseen(folder, s).sort(byRank);
      if (alerts.length <= ATTENTION_PER_FOLDER) {
        for (const alert of alerts) out.push({ folder, label, alert, count: 1 });
        continue;
      }
      const kinds = new Map();
      for (const a of alerts) kinds.set(a.kind, (kinds.get(a.kind) || 0) + 1);
      out.push({
        folder,
        label,
        count: alerts.length,
        alert: {
          kind: alerts[0].kind,
          subject: `${alerts.length} flywheel alerts`,
          detail: [...kinds].map(([kind, n]) => `${n} ${KIND_LABEL[kind] || kind}`).join(" · "),
          run_id: null,
        },
      });
    }
    out.sort((a, b) => byRank(a.alert, b.alert) || a.label.localeCompare(b.label));
    return out;
  }

  // --- header pill -----------------------------------------------------------

  function renderPill() {
    const pill = $("#fw-toggle");
    if (!pill) return;
    const session = ctx.sessionById(state.attached);
    const folder = folderOf(session);
    // On a phone the panel covers the session: switching to a session of
    // another folder (or none) closes it, as its pill would.
    if (state.attached !== ui.pillSession) {
      ui.pillSession = state.attached;
      if (ui.folder && ui.folder !== folder && window.matchMedia("(max-width: 760px)").matches) close();
    }
    pill.hidden = !folder;
    if (!folder) return;
    const s = summary(folder);
    pill.setAttribute("aria-pressed", String(ui.folder === folder));
    pill.dataset.tone = tone(s);
    const badge = $("#fw-count");
    const fresh = unseen(folder, s).length;
    badge.hidden = !fresh;
    badge.textContent = String(fresh);
    const key = ctx.keyOf ? ctx.keyOf("flywheel.toggle") : "";
    const hint = key ? ` (${key})` : "";
    pill.title = viaWorktree(session, folder)
      ? `Flywheel panel for ${labelFor(folder)}, this worktree's base checkout${hint}`
      : `Flywheel panel for ${labelFor(folder)}${hint}`;
  }

  function togglePill() {
    const folder = folderOf(ctx.sessionById(state.attached));
    if (!folder) return;
    if (ui.folder === folder) close();
    else open(folder, ctx.sessionById(state.attached).workspace_id);
  }

  // --- the drawer -------------------------------------------------------------

  function open(folder, workspaceId) {
    if (ui.folder !== folder) ui.panel = null;
    ui.folder = folder;
    ui.workspaceId = workspaceId || null;
    ui.error = "";
    ctx.onOpen();
    renderDrawer();
    $("#flywheel").focus({ preventScroll: true });
    loadAccounts(folder);
    const loaded = loadPanel();
    changed();
    return loaded;
  }

  // Open the panel of this hub's folder whose autopilot gate is `runId`
  // (a push's link names the run, never a path); false when none has it.
  async function openGate(runId) {
    for (const [key, s] of ui.summaries) {
      if (machineOfKey(key) || !(s.autopilot_gates > 0)) continue;
      let panel;
      try {
        panel = await api("GET", `/api/flywheel/panel?folder=${encodeURIComponent(key)}`);
      } catch (_) {
        continue;
      }
      if (panel.intake && panel.intake.gates.some((g) => g.run_id === runId)) {
        await open(key);
        return true;
      }
    }
    return false;
  }

  function close() {
    if (!ui.folder) return;
    ui.folder = null;
    ui.panel = null;
    renderDrawer();
    changed();
    ctx.focusTerminal();
  }

  $("#fw-close").addEventListener("click", close);
  $("#flywheel").addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !document.querySelector("dialog[open]")) {
      event.stopPropagation();
      close();
    }
  });

  // A drawer section; its heading folds it, remembered per title.
  function section(title, count, ...body) {
    const shut = collapsed.has(title);
    const content = el("div", { className: "fw-sec-body", hidden: shut }, ...body);
    const toggle = el(
      "button",
      {
        type: "button",
        className: "fw-sec-toggle",
        "aria-expanded": String(!shut),
        title: shut ? `Expand ${title}` : `Collapse ${title}`,
        dataset: { fwAction: "toggle-section" },
        onclick: () => {
          const fold = !collapsed.has(title);
          if (fold) collapsed.add(title);
          else collapsed.delete(title);
          store.set(COLLAPSED_KEY, [...collapsed]);
          content.hidden = fold;
          node.classList.toggle("collapsed", fold);
          toggle.setAttribute("aria-expanded", String(!fold));
          toggle.title = `${fold ? "Expand" : "Collapse"} ${title}`;
        },
      },
      icon("chev"),
      title,
      count !== undefined && count !== null ? el("span", { className: "fw-n", textContent: String(count) }) : null,
    );
    const node = el(
      "section",
      { className: `fw-sec${shut ? " collapsed" : ""}`, dataset: { fwSection: title } },
      el("h3", {}, toggle),
      content,
    );
    return node;
  }

  function outputButton(run_id, task) {
    if (!run_id) return null;
    return el("button", {
      type: "button",
      className: "text-btn fw-out",
      textContent: "output",
      title: `Open run ${run_id}'s output`,
      onclick: (event) => {
        event.stopPropagation();
        openRun(ui.folder, run_id, task);
      },
    });
  }

  function daemonRow(name, daemon, extra) {
    return el(
      "div",
      { className: `fw-daemon${daemon.live ? " live" : ""}` },
      el("i", { className: `fw-dot ${daemon.live ? "live" : "idle"}`, "aria-hidden": "true" }),
      el("b", { textContent: name }),
      el("span", { className: "fw-daemon-detail", textContent: daemon.detail }),
      extra || null,
    );
  }

  // POST answering status and body (api() keeps only the error text; a
  // 409 here carries the running worker).
  async function post(path, body) {
    const response = await fetch(path, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (response.status === 401) {
      window.location.replace("/" + window.location.hash);
      throw new Error("signed out");
    }
    let data = null;
    try {
      data = await response.json();
    } catch (_) {
      // An empty or non-JSON body: the status says it all.
    }
    return { status: response.status, ok: response.ok, data };
  }

  // Autopilot's row, section, and gate actions (autopilot.js).
  const pilot = window.FwHubAutopilot({
    el, icon, state, toast, confirmDialog, openSession, post,
    ago, span,
    pathOf, machineOfKey, labelFor, workspaceFor,
    section, outputButton,
    termSize: () => (ctx.termSize ? ctx.termSize() : {}),
    refresh: () => ctx.refresh(),
    close: () => close(),
    reload: () => loadPanel(),
    render: () => renderDrawer(),
  });

  // A panel task's prerequisites not verified yet (one the panel does not
  // list is archived, so done).
  // The prerequisites a scoped run would pull in, transitively: the ones
  // not done. A `parked` task is done (its graders passed; only its landing
  // parked), so the worker counts it as met, and so does this.
  function unmetPrerequisites(panel, taskId) {
    const tasks = new Map(panel.phases.flatMap((p) => p.tasks).map((t) => [t.id, t]));
    const done = (id) => ["verified", "parked"].includes(tasks.get(id).state);
    const unmet = [];
    const stack = [taskId];
    while (stack.length) {
      const task = tasks.get(stack.pop());
      if (!task) continue;
      for (const id of task.prerequisites) {
        if (!tasks.has(id) || done(id) || unmet.includes(id)) continue;
        unmet.push(id);
        stack.push(id);
      }
    }
    return unmet;
  }

  // Start a worker in the open panel's folder: the whole queue, or only
  // `taskId` (`fw worker --task`), with its unmet prerequisites if asked.
  // `where` (the board): its folder key, panel and workspace instead of
  // the open panel's; `exact` runs only `taskId`, never asking about its
  // prerequisites or another worker's leases (a running worker session of
  // this hub there is still asked about). True once started.
  async function startWorker(taskId, where) {
    const folder = where ? where.folder : ui.folder;
    const panel = where ? where.panel : ui.panel;
    const exact = !!(where && where.exact);
    const workspace = (where && where.workspaceId && workspaceById(where.workspaceId)) || workspaceFor(folder);
    if (!folder || !workspace) return false;
    const scoped = typeof taskId === "string" && taskId !== "";
    let withPrereqs = false;
    // What this worker runs, in order (for the confirms below).
    let runs = scoped ? [taskId] : [];
    if (scoped && panel && !exact) {
      const unmet = unmetPrerequisites(panel, taskId);
      if (unmet.length) {
        const ok = await confirmDialog({
          kicker: "flywheel",
          title: `Run ${taskId} with its prerequisites?`,
          body: `${taskId} waits on ${unmet.join(", ")}, not verified yet. The worker runs ${unmet.length === 1 ? "it" : "them"} first, then ${taskId}, and nothing else; it stops if one fails.`,
          ok: `Run ${unmet.length + 1} tasks`,
        });
        if (!ok) return false;
        withPrereqs = true;
        runs = [...unmet, taskId];
      }
    }
    let another = false;
    if (panel && panel.worker.live && !exact) {
      const ok = await confirmDialog({
        kicker: "flywheel",
        title: "Start another worker?",
        body: scoped
          ? `A worker already holds leases in ${labelFor(folder)} (${panel.worker.detail}). This one runs ${runs.length > 1 ? `${runs.slice(0, -1).join(", ")}, then ${taskId}` : `only ${taskId}`}; if the other claims ${runs.length > 1 ? "one" : "it"} first, this one waits for it, then reports.`
          : `A worker already holds leases in ${labelFor(folder)} (${panel.worker.detail}). Workers claim tasks, so a second one runs tasks in parallel with it.`,
        ok: "Start another",
      });
      if (!ok) return false;
      another = true;
    }
    try {
      const request = { folder, workspace_id: workspace.id, another };
      // The drawer's account pick (none for the CLI login or the board).
      const account = where ? "" : chosenAccount(folder);
      if (account) request.account = account;
      if (scoped) {
        request.tasks = [taskId];
        request.with_prereqs = withPrereqs;
      }
      const workerPath = `${via(folder)}/api/flywheel/worker`;
      request.folder = pathOf(folder);
      let out = await post(workerPath, request);
      if (out.status === 409 && out.data && out.data.running) {
        const running = out.data.running;
        const ok = await confirmDialog({
          kicker: "flywheel",
          title: "A worker is already running here",
          body: `Session "${running.name}" runs fw worker in ${labelFor(folder)}. A second worker claims tasks in parallel with it.`,
          ok: "Start another",
        });
        if (!ok) {
          close();
          openSession(running.id);
          return false;
        }
        out = await post(workerPath, { ...request, another: true });
      }
      if (!out.ok) throw new Error((out.data && out.data.error) || `HTTP ${out.status}`);
      const view = out.data;
      await ctx.refresh();
      // The worker's terminal is the thing to watch now: beside the pane
      // in focus (a new tab when there is no room), never over its agent.
      close();
      openSession(view.id, { split: "down" });
      toast(el("span", {}, "Started ", el("b", { textContent: view.name }), ` in ${labelFor(folder)}`), { timeout: 3500 });
      return true;
    } catch (error) {
      toast(`Could not start a worker: ${error.message}`, { tone: "bad" });
      return false;
    }
  }

  function phaseBar(tasks) {
    const counts = {};
    for (const t of tasks) counts[t.state] = (counts[t.state] || 0) + 1;
    return el(
      "div",
      { className: "fw-bar", role: "img", "aria-label": LIGHTS.filter((l) => counts[l]).map((l) => `${counts[l]} ${l}`).join(", ") },
      LIGHTS.filter((l) => counts[l]).map((l) => el("i", { className: `fw-seg ${l}`, style: `flex-grow:${counts[l]}`, title: `${counts[l]} ${l}` })),
    );
  }

  function taskRow(task) {
    const note = task.stranded
      ? `stranded: ${task.stranded.kind}${task.stranded.detail ? ` -- ${task.stranded.detail}` : ""}`
      : task.stopped
        ? `stopped: ${task.stopped.kind}${task.stopped.detail ? ` -- ${task.stopped.detail}` : ""}`
        : task.awaiting
          ? `awaiting: ${task.awaiting}`
          : task.blocked_on
            ? `blocked on ${task.blocked_on}`
            : task.error || (task.prerequisites.length && task.state === "pending" ? `after ${task.prerequisites.join(", ")}` : "");
    return el(
      "li",
      { className: `fw-task ${task.state}` },
      el("span", { className: `fw-light ${task.state}`, textContent: task.state }),
      el(
        "span",
        { className: "fw-task-text" },
        el("b", { textContent: task.id, title: task.id }),
        note ? el("small", { textContent: note, title: note }) : null,
        queuedIn(task),
      ),
      el(
        "span",
        { className: "fw-task-meta" },
        task.worker_id ? el("small", { textContent: task.worker_id, title: "worker" }) : null,
        runButton(task),
        fanOutButton(task),
        outputButton(task.run_id, task.id),
      ),
    );
  }

  // A pending task a running scoped worker will run: "queued in <its
  // session>" (this hub's: opens it).
  function queuedIn(task) {
    const queued = task.state === "pending" ? task.queued_in : null;
    if (!queued) return null;
    const text = `queued in ${queued.name}`;
    if (machineOfKey(ui.folder)) return el("small", { className: "fw-queued", textContent: text, title: text });
    return el("button", {
      type: "button",
      className: "text-btn fw-queued",
      textContent: text,
      title: `${queued.name} runs ${task.id} once its turn comes: open its terminal`,
      onclick: (event) => {
        event.stopPropagation();
        close();
        openSession(queued.session_id);
      },
    });
  }

  // "Run <id> with worker here": `fw worker --task <id>` in a hub shell
  // session, which runs that task alone and exits once it settles. This
  // hub's folders only (a peer hub that predates scoped workers would
  // drain its whole queue instead), and only work not verified or running.
  function runButton(task) {
    const folder = ui.folder;
    if (machineOfKey(folder) || task.state === "verified" || task.state === "running") return null;
    return el("button", {
      type: "button",
      className: "text-btn fw-out",
      textContent: "run",
      title: `Run ${task.id} with worker here: fw worker --task ${task.id} in a new shell session, which runs only this task${task.prerequisites.length ? " (and, if you confirm, its unmet prerequisites first)" : ""}, then exits`,
      "aria-label": `Run ${task.id} with worker here`,
      onclick: (event) => {
        event.stopPropagation();
        startWorker(task.id);
      },
    });
  }

  // "Fan out this task" (fanout-task.js): this hub's folders only, and
  // only work not verified yet.
  function fanOutButton(task) {
    const folder = ui.folder;
    if (!ctx.fanOutTask || machineOfKey(folder) || task.state === "verified") return null;
    return el("button", {
      type: "button",
      className: "text-btn fw-out",
      textContent: "fan out",
      title: `Fan out ${task.id}: several agents in worktrees on its goal, graded by the hub with its command graders and its held-out ones (never shown to the agents)`,
      onclick: (event) => {
        event.stopPropagation();
        const workspace = workspaceFor(folder);
        ctx.fanOutTask(pathOf(folder), workspace ? workspace.id : ui.workspaceId, task.id);
      },
    });
  }

  // "Run <id> with worker here" from elsewhere (a promoted fan-out): the
  // folder's panel, then its scoped start -- once the panel is read, so a
  // worker already holding leases there (one started outside the hub
  // too) and unmet prerequisites are asked about first. No `taskId`
  // starts the unscoped worker.
  async function startWorkerIn(folder, workspaceId, taskId) {
    await open(folder, workspaceId);
    return startWorker(taskId);
  }

  // The active tasks in `folder` not verified yet, other than `except`:
  // what `fw worker` there drains besides it (it takes no task).
  async function pendingTasks(folder, except) {
    const panel = await api("GET", `${via(folder)}/api/flywheel/panel?folder=${encodeURIComponent(pathOf(folder))}`);
    const ids = [];
    for (const phase of panel.phases || []) {
      for (const t of phase.tasks || []) {
        if (t.id !== except && t.state !== "verified" && !ids.includes(t.id)) ids.push(t.id);
      }
    }
    return ids;
  }

  function graderChips(graders) {
    if (!graders.length) return el("span", { className: "fw-nograders", textContent: "no receipts" });
    return el(
      "span",
      { className: "fw-graders" },
      graders.map((g) => el("span", {
        className: `fw-grader ${g.passed ? "pass" : "fail"}`,
        textContent: g.name,
        title: `${g.kind} grader ${g.name}: ${g.passed ? "pass" : "FAIL"} (${g.duration_ms} ms)`,
      })),
    );
  }

  // A run's status in the tasks' vocabulary: a DONE run passed its
  // graders, which is what "verified" means on a task.
  // (A parked gate is "awaiting": the full word overflows its pill.)
  const runLabel = (status) => (status === "done" ? "verified" : status === "awaiting_approval" ? "awaiting" : status.replace("_", " "));

  function runRow(run) {
    return el(
      "li",
      { className: `fw-run ${run.status}` },
      el(
        "div",
        { className: "fw-run-line" },
        el("span", { className: `fw-status ${run.status}`, textContent: runLabel(run.status), title: `lifecycle ${run.status}${run.outcome ? `, ${run.outcome}` : ""}` }),
        el("b", { textContent: run.task_id, title: run.run_id }),
        el("span", { className: "fw-run-when", textContent: run.updated_at ? `${ago(run.updated_at)} ago` : "" }),
        outputButton(run.run_id, run.task_id),
      ),
      el(
        "div",
        { className: "fw-run-sub" },
        graderChips(run.graders),
        el("small", {
          className: "fw-run-cost",
          textContent: `${run.attempts} attempt${run.attempts === 1 ? "" : "s"} · ${tokens(run.tokens)} tok · ${dollars(run.cost_usd)}`,
        }),
      ),
      run.error ? el("small", { className: "fw-run-error", textContent: run.error, title: run.error }) : null,
    );
  }

  function liveRow(run) {
    const idle = run.idle_seconds;
    return el(
      "li",
      { className: `fw-run live${idle > 90 ? " stale" : ""}` },
      el(
        "div",
        { className: "fw-run-line" },
        el("span", { className: `fw-status ${run.status}`, textContent: runLabel(run.status), title: `lifecycle ${run.status}` }),
        el("b", { textContent: run.task_id, title: run.run_id }),
        el("span", {
          className: "fw-run-when",
          textContent: idle !== null && idle !== undefined ? `idle ${span(idle)}${idle > 90 ? " STALE" : ""}` : "",
        }),
        outputButton(run.run_id, run.task_id),
      ),
      el("small", {
        className: "fw-run-cost",
        textContent: [
          run.attempt ? `attempt ${run.attempt}` : null,
          run.iteration ? `iter ${run.iteration}` : null,
          `${tokens(run.tokens)} tok`,
          dollars(run.cost_usd),
          run.worker_id,
          run.awaiting ? `awaiting: ${run.awaiting}` : run.last_detail,
        ].filter(Boolean).join(" · "),
      }),
    );
  }

  function alertRow(alert) {
    return el(
      "li",
      { className: `fw-alert ${alert.kind}${isSeen(ui.folder, alert) ? " seen" : ""}` },
      el("span", { className: `fw-kind ${alert.kind}`, textContent: KIND_LABEL[alert.kind] || alert.kind }),
      el(
        "span",
        { className: "fw-task-text" },
        el("b", { textContent: alert.subject }),
        el("small", { textContent: alert.detail, title: alert.detail }),
      ),
      el(
        "span",
        { className: "fw-task-meta" },
        ui.panel ? pilot.alertActions(ui.folder, ui.panel, alert) : null,
        outputButton(alert.run_id, alert.subject),
      ),
    );
  }

  function claimRow(claim) {
    const lease = Date.parse(claim.lease_expires_at) - Date.now();
    return el(
      "li",
      { className: `fw-claim${claim.live ? " live" : ""}` },
      el("b", { textContent: claim.task_id }),
      el("span", { textContent: claim.worker_id }),
      el("small", {
        textContent: claim.live ? `lease ${span(lease / 1000)} left` : `lapsed ${ago(claim.lease_expires_at)} ago`,
      }),
    );
  }

  function renderDrawer() {
    const drawer = $("#flywheel");
    drawer.hidden = !ui.folder;
    if (!ui.folder) return;
    const folder = ui.folder;
    const panel = ui.panel && ui.panel.folder === folder ? ui.panel : null;
    $("#fw-title").textContent = labelFor(folder);
    // Cut from its start, as the header's cwd meter is.
    $("#fw-path").textContent = window.FwHubFmt.pathTail(pathOf(folder));
    $("#fw-path").title = machineOfKey(folder) ? `${pathOf(folder)} on ${machineOfKey(folder)}` : folder;
    const workers = workerSessions(folder);
    const body = $("#fw-body");
    if (!panel) {
      body.replaceChildren(el("p", { className: "fw-empty", textContent: ui.error || "Reading the loop's state..." }));
      $("#fw-read").textContent = "";
      return;
    }
    stamp();
    // No source read: the policy itself does not load (a worker would exit
    // at once with the same error).
    const broken = !!panel.problem && !panel.source;
    // An unapproved .flywheel.toml: a worker would refuse at once.
    const untrusted = panel.untrusted || null;
    const start = el(
      "button",
      {
        type: "button",
        className: "slab fw-start",
        onclick: () => startWorker(),
        disabled: broken || !!untrusted,
        title: untrusted
          ? `Approve .flywheel.toml first: ${untrusted.command}`
          : broken
          ? "The project does not resolve (see below): fix its config, then start a worker"
          : "Run fw worker in a new shell session in this folder: it drains every active task not done yet (a task's run button runs that one alone)",
      },
      icon("plus"),
      untrusted ? "Run fw trust first" : broken ? "Fix the config first" : panel.worker.live ? "Start another worker" : "Start worker here",
    );
    const accountPick = accountSelect(folder, broken || !!untrusted);
    const goto = workers.length
      ? el("button", {
        type: "button",
        className: "text-btn",
        textContent: workers.length === 1 ? "open worker session" : `${workers.length} worker sessions`,
        onclick: () => openSession(workers[0].id),
      })
      : null;
    const tasks = panel.phases.flatMap((p) => p.tasks);
    const verified = tasks.filter((t) => t.state === "verified").length;
    const children = [
      el(
        "div",
        { className: "fw-daemons" },
        daemonRow("worker", panel.worker, el("span", { className: "fw-daemon-actions" }, goto, accountPick, start)),
        panel.intake ? pilot.row(folder, panel, daemonRow) : daemonRow("autopilot", panel.autopilot),
        el("div", { className: "fw-where" }, el("span", { textContent: panel.source }), el("span", { textContent: panel.store })),
      ),
    ];
    if (ui.error) children.push(el("p", { className: "fw-problem", role: "alert", textContent: ui.error }));
    if (untrusted) {
      children.push(el(
        "p",
        { className: "fw-problem fw-untrusted", role: "alert" },
        `${untrusted.file} is not trusted: its settings do not apply, and workers and the autopilot refuse to run here. Review it, then run in a terminal: `,
        el("code", { textContent: untrusted.command }),
      ));
    }
    const note = worktreeNote(panel);
    if (note) children.push(el("p", { className: "fw-note", textContent: note }));
    if (panel.problem) children.push(el("p", { className: "fw-problem", textContent: panel.problem }));
    if (panel.pauses && panel.pauses.length) children.push(pausesSection(folder, panel.pauses));
    if (panel.alerts.length) children.push(alertsSection(folder, panel.alerts));
    const autopilot = pilot.section(folder, panel);
    if (autopilot) children.push(autopilot);
    if (panel.live.length) {
      children.push(section("In flight", panel.live.length, el("ul", { className: "fw-list" }, panel.live.map(liveRow))));
    }
    if (panel.tasks_note) children.push(section("Tasks", null, el("p", { className: "fw-empty", textContent: panel.tasks_note })));
    else if (!panel.phases.length) {
      children.push(section("Active phases", 0, el("p", { className: "fw-empty", textContent: "No active tasks: every phase is archived (or none was planned yet)." })));
    } else {
      children.push(section(
        "Active phases",
        `${verified}/${tasks.length} verified`,
        ...panel.phases.map((phase) => el(
          "div",
          { className: "fw-phase" },
          el(
            "div",
            { className: "fw-phase-head" },
            el("b", { textContent: phase.name }),
            el("small", { textContent: `${phase.tasks.filter((t) => t.state === "verified").length}/${phase.tasks.length}` }),
          ),
          phaseBar(phase.tasks),
          el("ul", { className: "fw-list" }, phase.tasks.map(taskRow)),
        )),
      ));
    }
    children.push(section(
      "Recent runs",
      panel.runs.length,
      panel.runs.length
        ? el("ul", { className: "fw-list" }, panel.runs.map(runRow))
        : el("p", { className: "fw-empty", textContent: "No runs recorded yet." }),
    ));
    if (panel.claims.length) {
      children.push(section("Claims", panel.claims.length, el("ul", { className: "fw-list fw-claims" }, panel.claims.map(claimRow))));
    }
    const scroll = body.scrollTop;
    // A clear reason being typed keeps its focus across a re-render.
    const typing = document.activeElement && document.activeElement.closest("[data-fw-pause]");
    const typingId = typing && document.activeElement.matches("input") ? typing.dataset.fwPauseId : null;
    body.replaceChildren(...children);
    body.scrollTop = scroll;
    if (typingId) {
      const input = body.querySelector(`[data-fw-pause-id="${CSS.escape(typingId)}"] input`);
      if (input) input.focus({ preventScroll: true });
    }
  }

  // --- start-worker account ------------------------------------------------

  // Folder `key`'s workspace's Claude accounts (read through its machine's
  // hub), for the start-worker picker; the pick starts as the workspace's
  // remembered one (launcher.js's FwHubAccounts).
  async function loadAccounts(key) {
    const workspace = (ui.workspaceId && workspaceById(ui.workspaceId)) || workspaceFor(key);
    const workspaceId = workspace ? workspace.id : null;
    const list = (await window.FwHubAccounts.list(api, `${via(key)}/api/claude-accounts`)) || [];
    if (ui.folder !== key) return;
    const remembered = workspaceId ? window.FwHubAccounts.remembered(workspaceId) : "";
    const pick = list.some((a) => a.name === remembered) ? remembered : "";
    ui.accounts = { folder: key, workspaceId, list, pick };
    renderDrawer();
  }

  // The account the drawer starts a worker in folder `key` under: "" for
  // the CLI login.
  function chosenAccount(key) {
    const a = ui.accounts;
    return a && a.folder === key ? a.pick : "";
  }

  function accountSelect(key, disabled) {
    const a = ui.accounts && ui.accounts.folder === key ? ui.accounts : { list: [], pick: "", workspaceId: null };
    const select = el("select", {
      className: "fw-account",
      disabled,
      title: "Claude account the worker's agents run under (fw worker --account); CLI login uses the CLI's own",
      "aria-label": "Worker account",
    });
    select.setAttribute("data-fw-worker-account", "");
    window.FwHubAccounts.fill(select, el, a.list, a.workspaceId, a.pick);
    select.addEventListener("change", () => {
      a.pick = select.value;
      if (a.workspaceId) window.FwHubAccounts.remember(a.workspaceId, select.value);
    });
    return select;
  }

  // --- session-limit pauses ------------------------------------------------

  // The panel's active pauses (`fw pauses --json` entries), each with a
  // clear that takes a reason; cleared elsewhere, one leaves with the
  // next panel read.
  function pausesSection(folder, pauses) {
    const live = new Set(pauses.map((p) => String(p.id)));
    for (const id of ui.pauseReasons.keys()) if (!live.has(id)) ui.pauseReasons.delete(id);
    return section("Session pauses", pauses.length, el("ul", { className: "fw-list fw-pauses" }, pauses.map((p) => pauseRow(folder, p))));
  }

  function pauseLabel(account) {
    return account === null || account === undefined ? "all accounts" : account === "" ? "cli login" : `@${account}`;
  }

  function pauseRow(folder, pause) {
    const id = String(pause.id);
    const reason = el("input", {
      type: "text",
      placeholder: "reason",
      value: ui.pauseReasons.get(id) || "",
      "aria-label": `Reason to clear pause ${id}`,
      spellcheck: false,
    });
    const clear = el("button", { type: "button", className: "text-btn", textContent: "clear", title: "Clear this pause (fw pauses clear): the reason is recorded" });
    reason.addEventListener("input", () => ui.pauseReasons.set(id, reason.value));
    const go = () => clearPause(folder, pause, reason, clear);
    reason.addEventListener("keydown", (event) => {
      if (event.key === "Enter") go();
    });
    clear.addEventListener("click", go);
    const row = el(
      "li",
      { className: "fw-pause" },
      el("b", { textContent: pauseLabel(pause.account) }),
      el("span", { textContent: `until ${clock(pause.pause_until)}`, title: `${pause.pause_until} (run ${pause.run_id})` }),
      el("span", { className: "fw-pause-clear" }, reason, clear),
    );
    row.setAttribute("data-fw-pause", "");
    row.setAttribute("data-fw-pause-id", id);
    // A pre-account row (account null, holding back every account) has no
    // account attribute; "" is the CLI login.
    if (typeof pause.account === "string") row.setAttribute("data-fw-pause-account", pause.account);
    row.setAttribute("data-fw-pause-until", pause.pause_until);
    return row;
  }

  // Clear one pause with the typed reason; an empty reason sends nothing.
  async function clearPause(folder, pause, input, button) {
    const reason = input.value.trim();
    if (!reason) {
      input.focus();
      return;
    }
    button.disabled = true;
    try {
      const out = await post(`${via(folder)}/api/flywheel/pauses/clear`, { folder: pathOf(folder), ids: [pause.id], reason });
      if (!out.ok) throw new Error((out.data && out.data.error) || `HTTP ${out.status}`);
      ui.pauseReasons.delete(String(pause.id));
      toast(`Cleared the ${pauseLabel(pause.account)} pause`, { timeout: 3000 });
      await loadPanel();
    } catch (error) {
      button.disabled = false;
      toast(`Could not clear the pause: ${error.message}`, { tone: "bad" });
    }
  }

  // The panel's Needs you: unseen alerts first, a seen toggle, and long
  // runs of one kind folded.
  function alertsSection(folder, alerts) {
    const fresh = alerts.filter((a) => !isSeen(folder, a)).sort(byRank);
    const old = alerts.filter((a) => isSeen(folder, a)).sort(byRank);
    const toggle = el("button", {
      type: "button",
      className: "text-btn fw-seen",
      textContent: fresh.length ? "mark seen" : "unmark seen",
      title: fresh.length
        ? "Take these out of the sidebar's Needs you list (this browser) until they change"
        : "Put these back in the sidebar's Needs you list",
      onclick: () => markSeen(folder, fresh.length > 0),
    });
    const shown = [...fresh, ...old];
    const head = shown.slice(0, 8);
    const rest = shown.slice(8);
    return section(
      "Needs you",
      fresh.length ? `${fresh.length} new${old.length ? ` · ${old.length} seen` : ""}` : `${old.length} seen`,
      el("div", { className: "fw-sec-actions" }, toggle),
      el("ul", { className: "fw-list" }, head.map(alertRow)),
      rest.length
        ? el(
          "details",
          { className: "fw-more" },
          el("summary", { textContent: `${rest.length} more` }),
          el("ul", { className: "fw-list" }, rest.map(alertRow)),
        )
        : null,
    );
  }

  function stamp() {
    if (!ui.panel) return;
    $("#fw-read").textContent = `read ${ago(ui.panel.generated_at)} ago`;
    $("#fw-read").title = `${ui.panel.source} · ${ui.panel.store}`;
  }

  // "read Ns ago" moves while the drawer shows; past the monitor's
  // re-read cadence the panel re-fetches its latest read.
  setInterval(() => {
    if (!ui.folder || !ui.panel || document.visibilityState !== "visible") return;
    stamp();
    if (!ui.loading && Date.now() - Date.parse(ui.panel.generated_at) > PANEL_STALE_MS) loadPanel();
  }, 5000);

  // --- run output viewer -------------------------------------------------------

  const runDialog = $("#dlg-run");

  // `opts.say`: focus the message box once the run reads as live.
  function openRun(folder, runId, task, opts = {}) {
    if (ui.run) clearTimeout(ui.run.timer);
    ui.run = { folder, runId, task, next: 0, from: 0, entries: [], summary: "", status: null, active: false, tab: "transcript", timer: null, error: "", seq: 0, shown: null, focusSay: !!opts.say };
    runDialog.setAttribute("data-fw-run-viewer", runId);
    $("#run-title").textContent = task || runId;
    $("#run-meta").textContent = runId;
    $("#run-body").replaceChildren(el("p", { className: "fw-empty", textContent: "Reading the run..." }));
    sayBar(ui.run);
    selectTab("transcript");
    if (!runDialog.open) runDialog.showModal();
    pollRun();
  }

  // The live run's message bar: say (POST /api/flywheel/steer/say) and
  // interrupt, which sends nothing until its confirm. This hub's folders
  // only (peers are refused steering); shown while the run reads live.
  function sayBar(run) {
    const foot = $("#run-say");
    if (machineOfKey(run.folder)) {
      foot.hidden = true;
      foot.replaceChildren();
      return;
    }
    const input = el("input", {
      type: "text",
      maxLength: 4000,
      placeholder: "Tell the agent something: it arrives as its next turn",
      "aria-label": "Message to the agent",
      "data-fw-input": "message",
      onkeydown: (event) => {
        if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
          event.preventDefault();
          send();
        }
      },
    });
    const note = el("p", { className: "run-say-note", role: "status" });
    const tell = (text, bad) => {
      note.textContent = text;
      note.classList.toggle("bad", !!bad);
    };
    const sendBtn = el("button", { type: "button", className: "slab", "data-fw-action": "say", onclick: () => send() }, icon("send"), "Send");
    const confirmBox = el("span", { className: "run-stop-confirm", hidden: true });
    const stop = el("button", {
      type: "button",
      className: "slab ghost run-stop",
      title: "Stops the agent mid-step; the run waits for you (asks first)",
      "data-fw-action": "interrupt",
      onclick: () => {
        if (!confirmBox.hidden) return;
        confirmBox.replaceChildren(
          el("span", { textContent: "Stop the agent mid-step?" }),
          el("button", { type: "button", className: "text-btn", textContent: "cancel", onclick: disarm }),
          el("button", { type: "button", className: "slab danger", "data-fw-action": "interrupt-confirm", onclick: interrupt }, icon("stop"), "Confirm interrupt"),
        );
        confirmBox.hidden = false;
        stop.hidden = true;
        confirmBox.querySelector(".danger").focus();
      },
    }, icon("stop"), "Interrupt");
    function disarm() {
      confirmBox.hidden = true;
      confirmBox.replaceChildren();
      stop.hidden = false;
    }
    async function steer(verb, body) {
      const out = await post(`/api/flywheel/steer/${verb}`, { folder: pathOf(run.folder), run: run.runId, ...body });
      if (!out.ok) throw new Error((out.data && out.data.error) || `HTTP ${out.status}`);
      return out.data;
    }
    async function send() {
      const text = input.value.trim();
      if (!text) {
        input.focus();
        return;
      }
      if (sendBtn.disabled) return;
      sendBtn.disabled = true;
      tell("Sending...");
      try {
        const done = await steer("say", { message: text });
        if (ui.run !== run) return;
        input.value = "";
        tell(`Queued as command #${done.command_id}: the agent gets it as its next turn.`);
      } catch (error) {
        if (ui.run === run) tell(`Not sent: ${error.message}`, true);
      } finally {
        sendBtn.disabled = false;
      }
    }
    async function interrupt() {
      const go = confirmBox.querySelector("[data-fw-action=interrupt-confirm]");
      if (!go || go.disabled) return;
      go.disabled = true;
      tell("Interrupting...");
      try {
        const done = await steer("interrupt", {});
        if (ui.run !== run) return;
        tell(`Interrupt queued (command #${done.command_id}): the run stops at its next step and waits for you.`);
      } catch (error) {
        if (ui.run === run) tell(`Not interrupted: ${error.message}`, true);
      }
      if (ui.run === run) disarm();
    }
    foot.replaceChildren(el("div", { className: "run-say-row" }, input, sendBtn, stop, confirmBox), note);
    foot.hidden = true;
  }

  async function pollRun() {
    const run = ui.run;
    if (!run || !runDialog.open) return;
    const seq = ++run.seq;
    try {
      const out = await api(
        "GET",
        `${via(run.folder)}/api/flywheel/run?folder=${encodeURIComponent(pathOf(run.folder))}&run=${encodeURIComponent(run.runId)}&since=${run.next}`,
      );
      if (ui.run !== run || seq !== run.seq) return;
      const restart = out.from > run.next || run.next === 0;
      let dropped = 0;
      if (restart) {
        // The first read (or a gap past the retained window): start over.
        run.entries = out.entries.slice(-RUN_WINDOW);
        run.from = out.from + (out.entries.length - run.entries.length);
      } else {
        run.entries.push(...out.entries);
        dropped = Math.max(0, run.entries.length - RUN_WINDOW);
        if (dropped) {
          run.entries.splice(0, dropped);
          run.from += dropped;
        }
      }
      const summaryChanged = run.summary !== out.summary;
      const hadError = !!run.error;
      run.next = out.next;
      run.summary = out.summary;
      run.status = out.status;
      run.active = out.active;
      run.error = "";
      if (restart || hadError || (run.tab === "summary" && summaryChanged)) renderRun(true);
      else if (out.entries.length) appendEntries(out.entries, dropped);
      else renderMeta();
    } catch (error) {
      if (ui.run !== run) return;
      run.error = error.message;
      renderRun(false);
    }
    if (ui.run === run && run.active && runDialog.open) run.timer = setTimeout(pollRun, RUN_POLL_MS);
  }

  function selectTab(tab) {
    if (ui.run) ui.run.tab = tab;
    for (const name of ["transcript", "summary"]) {
      const button = $(`#run-tab-${name}`);
      button.setAttribute("aria-selected", String(name === tab));
    }
    renderRun(true);
    if (tab === "summary") $("#run-body").scrollTop = 0;
  }
  $("#run-tab-transcript").addEventListener("click", () => selectTab("transcript"));
  $("#run-tab-summary").addEventListener("click", () => selectTab("summary"));
  runDialog.addEventListener("close", () => {
    if (ui.run) clearTimeout(ui.run.timer);
    ui.run = null;
    runDialog.removeAttribute("data-fw-run-viewer");
    $("#run-say").hidden = true;
    $("#run-say").replaceChildren();
  });

  function entryNode(entry) {
    const body = entry.body || "";
    const lines = body.split("\n");
    const long = entry.kind === "tool_result" && lines.length > 4;
    const text = long
      ? el(
        "details",
        { className: "run-fold" },
        el("summary", { textContent: `${lines[0].slice(0, 160)}  (+${lines.length - 1} lines)` }),
        el("pre", { className: "run-text", textContent: body }),
      )
      : body ? el("pre", { className: "run-text", textContent: body }) : null;
    return el(
      "div",
      { className: `run-entry ${entry.kind}${entry.error ? " error" : ""}`, "data-fw-entry": "" },
      el("span", { className: "run-ts", textContent: clock(entry.ts) }),
      el("span", { className: "run-header", textContent: entry.header }),
      text,
    );
  }

  function renderMeta() {
    const run = ui.run;
    $("#run-meta").textContent = [run.runId, run.status, run.active ? "live" : null].filter(Boolean).join(" · ");
    $("#run-follow-wrap").hidden = !run.active;
    // The message bar stays up while a refusal it just got is showing.
    const foot = $("#run-say");
    if (foot.firstChild) {
      const refused = foot.querySelector(".run-say-note.bad");
      foot.hidden = !run.active && !(refused && refused.textContent);
      if (run.active && run.focusSay) {
        run.focusSay = false;
        foot.querySelector("[data-fw-input=message]").focus();
      }
    }
  }

  const earlierText = (from) => `${from} earlier entries not shown (the newest are).`;

  function renderRun(scrollToEnd) {
    const run = ui.run;
    if (!run) return;
    const body = $("#run-body");
    renderMeta();
    run.shown = null;
    if (run.tab === "summary") {
      body.replaceChildren(...[
        run.error ? el("p", { className: "fw-problem", textContent: run.error }) : null,
        el("pre", { className: "run-summary", textContent: run.summary || "" }),
      ].filter(Boolean));
      return;
    }
    const nearEnd = body.scrollHeight - body.scrollTop - body.clientHeight < 80;
    const nodes = [];
    if (run.error) nodes.push(el("p", { className: "fw-problem", textContent: run.error }));
    const earlier = el("p", { className: "fw-empty run-earlier", textContent: earlierText(run.from) });
    earlier.hidden = run.from === 0;
    nodes.push(earlier);
    if (!run.entries.length && !run.error) {
      nodes.push(el("p", { className: "fw-empty", textContent: run.active ? "No transcript yet: waiting for the agent." : "This run left no transcript in the logs (see the summary)." }));
    }
    const list = el("div", { className: "run-entries" }, run.entries.map(entryNode));
    nodes.push(list);
    body.replaceChildren(...nodes);
    // Later polls append to this list instead of rebuilding it.
    if (run.entries.length && !run.error) run.shown = { list, earlier };
    if (scrollToEnd && ($("#run-follow").checked || nearEnd)) {
      body.scrollTop = body.scrollHeight;
    }
  }

  // New transcript entries onto the shown list (the oldest `dropped` off
  // it): a long live run costs the DOM only what changed.
  function appendEntries(fresh, dropped) {
    const run = ui.run;
    if (!run || run.tab !== "transcript" || !run.shown) {
      renderRun(true);
      return;
    }
    const body = $("#run-body");
    const nearEnd = body.scrollHeight - body.scrollTop - body.clientHeight < 80;
    const { list, earlier } = run.shown;
    for (let i = 0; i < dropped && list.firstChild; i++) list.firstChild.remove();
    list.append(...fresh.slice(-RUN_WINDOW).map(entryNode));
    earlier.hidden = run.from === 0;
    earlier.textContent = earlierText(run.from);
    renderMeta();
    if ($("#run-follow").checked || nearEnd) body.scrollTop = body.scrollHeight;
  }

  // --- palette ----------------------------------------------------------------

  function paletteItems(cmd) {
    for (const folder of ui.summaries.keys()) {
      const s = summary(folder);
      const n = unseen(folder, s).length;
      const tasks = total(s) ? `${s.counts.verified || 0}/${total(s)} verified` : "no active tasks";
      cmd(`Flywheel: ${labelFor(folder)}`, n ? `${n} need${n === 1 ? "s" : ""} you` : tasks, () => open(folder), "wheel");
    }
  }

  return {
    reload,
    reloadMachine,
    onEvent,
    folderChip,
    attention,
    open,
    close,
    togglePill,
    renderPill,
    render: () => {
      renderPill();
      if (ui.folder) renderDrawer();
    },
    paletteItems,
    isOpen: () => !!ui.folder,
    openRun,
    openGate,
    startWorkerIn,
    pendingTasks,
  };
};

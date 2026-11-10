// flywheel hub: the flywheel board's task drawer and New task form
// (board.js creates it). The drawer leads with the task's state and what
// can be done about it -- run it alone (a worker scoped to it), approve
// or reject its parked gate, watch, message or interrupt its live run
// (the run viewer, flywheel.js) -- then its goal, its checks (held-out
// ones by name only: the hub never serves their commands), and every run
// it has had, each attempt's outcome and error. New task plans a task
// file and writes it only on confirm. Data: GET /api/flywheel/history
// (re-read with the board's panel), the plan's panel; actions: POST
// /api/flywheel/worker, /api/flywheel/autopilot/decide (autopilot.js's
// shared gate state), /api/flywheel/tasks. Hooks for checks: data-fw-*.
"use strict";

window.FwHubBoardTask = function (ctx) {
  const { el, icon, span, age, flywheel, toast } = ctx;
  const { dollars, tokens, plural } = window.FwHubFmt;

  const POLL_MS = 4000;

  // The open task: { key, id, plan, history, error, seq, loading,
  // feedback, note: { text, bad } } | null.
  let cur = null;
  let html = null;

  const node = el("aside", {
    className: "commands fw-drawer fwb-drawer",
    hidden: true,
    tabIndex: -1,
    "aria-label": "Task",
    onkeydown: (event) => {
      if (event.key === "Escape" && cur && !document.querySelector("dialog[open]")) {
        event.stopPropagation();
        close();
      }
    },
  });
  const kicker = el("span", { className: "sheet-kicker" });
  const title = el("h2");
  const body = el("div", { className: "fw-body fwb-task-body" });
  node.append(
    el(
      "header",
      { className: "cmd-head" },
      el("div", { className: "cmd-title" }, kicker, title),
      el("button", { type: "button", className: "icon", title: "Close (Esc)", "aria-label": "Close the task", onclick: () => close() }, icon("x")),
    ),
    body,
  );

  const local = (key) => !flywheel.machineOfKey(key);

  // --- open, close, read ---------------------------------------------------

  function open(key, id, plan) {
    const again = cur && cur.key === key && cur.id === id;
    if (!again) cur = { key, id, plan, history: null, error: "", seq: 0, loading: false, feedback: "", note: null };
    else cur.plan = plan;
    html = null;
    if (!again) ctx.onOpen();
    node.hidden = false;
    node.setAttribute("data-fw-drawer", id);
    render();
    body.scrollTop = 0;
    node.focus({ preventScroll: true });
    ctx.onChange();
    load();
  }

  function close() {
    if (!cur) return;
    const id = cur.id;
    cur = null;
    html = null;
    node.hidden = true;
    node.removeAttribute("data-fw-drawer");
    body.replaceChildren();
    ctx.onChange();
    const card = ctx.root() && ctx.root().querySelector(`[data-fw-task="${CSS.escape(id)}"]`);
    if (card) card.focus({ preventScroll: true });
  }

  async function load() {
    const mine = cur;
    if (!mine) return;
    const seq = ++mine.seq;
    mine.loading = true;
    try {
      const history = await flywheel.readHistory(mine.key, mine.id);
      if (cur !== mine || seq !== mine.seq) return;
      mine.history = history;
      mine.error = "";
    } catch (error) {
      if (cur !== mine || seq !== mine.seq) return;
      mine.error = error.message;
    }
    mine.loading = false;
    render();
  }

  setInterval(() => {
    if (!cur || cur.loading || !node.isConnected || document.visibilityState !== "visible") return;
    load();
  }, POLL_MS);

  // Folder `key` changed (a push): the open task re-reads.
  function refresh(key) {
    if (cur && (key === null || cur.key === key)) load();
  }

  // --- the task as the plan's panel knows it ---------------------------------

  function known() {
    const panel = ctx.panelOf(cur.key);
    if (!panel) return { panel: null, task: null, info: null, live: null, states: new Map() };
    const states = new Map();
    for (const p of panel.plans || []) for (const t of p.tasks || []) states.set(t.id, t.state);
    const plan = (panel.plans || []).find((p) => p.name === cur.plan) || null;
    const task = plan && plan.tasks ? plan.tasks.find((t) => t.id === cur.id) || null : null;
    const info = panel.phases.flatMap((ph) => ph.tasks).find((t) => t.id === cur.id) || null;
    const live = panel.live.find((r) => r.task_id === cur.id) || null;
    return { panel, plan, task, info, live, states };
  }

  // --- render ----------------------------------------------------------------

  function render() {
    if (!cur || !node.isConnected) return;
    const k = known();
    const state = (k.task && k.task.state) || (k.info && k.info.state) || "pending";
    const status = (k.task && k.task.status) || (k.live && k.live.status) || (k.info && k.info.run_status) || null;
    const lane = ctx.laneOf(state, status);
    kicker.textContent = `task · ${cur.plan}${k.plan && k.plan.archived ? " (archived)" : ""}`;
    title.textContent = cur.id;
    const h = cur.history;
    const fresh = el(
      "div",
      {},
      cur.error ? el("p", { className: "fw-problem", role: "alert", textContent: h ? `Not re-read: ${cur.error}` : cur.error }) : null,
      band(k, state, status, lane),
      goalSection(k, h),
      checksSection(h),
      runsSection(h),
    );
    const next = fresh.innerHTML;
    if (next === html) return;
    html = next;
    const active = document.activeElement;
    const hook = active && body.contains(active) ? hookOf(active) : null;
    const caret = hook && "selectionStart" in active ? [active.selectionStart, active.selectionEnd] : null;
    const scroll = body.scrollTop;
    body.replaceChildren(...fresh.childNodes);
    body.scrollTop = scroll;
    if (hook) {
      const again = body.querySelector(`[${hook.name}="${CSS.escape(hook.value)}"]`);
      if (again) {
        again.focus({ preventScroll: true });
        if (caret && "setSelectionRange" in again) again.setSelectionRange(caret[0], caret[1]);
      }
    }
  }

  function hookOf(n) {
    for (const attr of n.attributes) {
      if (/^data-fw-(action|input|run|check)$/.test(attr.name)) return { name: attr.name, value: attr.value };
    }
    return null;
  }

  const OUTCOME_WORDS = { succeeded: "verified", failed_validation: "checks failed", validation_failed: "checks failed", agent_error: "agent aborted", internal_error: "harness error" };
  const outcomeWords = (o) => (o ? OUTCOME_WORDS[o] || o.replace(/_/g, " ") : "in flight");
  const runLabel = (s) => (s === "done" ? "verified" : s === "awaiting_approval" ? "awaiting" : s.replace(/_/g, " "));

  function bandHead(dot, head, meta) {
    return el("div", { className: "fwb-agent-line" }, dot, el("b", { textContent: head }), meta ? el("small", { textContent: meta }) : null);
  }

  // The run viewer on run `runId` of the open task.
  const watch = (runId, opts) => () => flywheel.openRun(cur.key, runId, cur.id, opts);

  function runButton(k, state, label, quiet) {
    if (!local(cur.key) || state === "verified" || state === "running" || !k.panel) return null;
    if (k.plan && k.plan.archived) return null;
    return el(
      "button",
      {
        type: "button",
        className: quiet ? "slab ghost" : "slab",
        "data-fw-action": "run",
        title: `Start fw worker --task ${cur.id} in a shell session here: it runs only ${cur.id}, then exits`,
        onclick: (event) => runTask(event.currentTarget),
      },
      icon("wheel"),
      label,
    );
  }

  async function runTask(button) {
    const k = known();
    if (!k.panel || button.disabled) return;
    button.disabled = true;
    const w = ctx.workspace();
    try {
      await flywheel.runTask(cur.key, cur.id, k.panel, w ? w.id : null);
    } finally {
      button.disabled = false;
    }
  }

  // Its state and what can be done about it, loudest first.
  function band(k, state, status, lane) {
    const h = cur.history;
    const latest = h && h.runs.length ? h.runs[0] : null;
    if (lane === "running" || lane === "checking") {
      const run = k.live;
      const runId = (run && run.run_id) || (latest && latest.run_id) || (k.info && k.info.run_id);
      const call = run && run.last_tool;
      return el(
        "div",
        { className: "fwb-agent live" },
        bandHead(
          el("i", { className: `fw-dot ${lane === "checking" ? "idle" : "live"}`, "aria-hidden": "true" }),
          lane === "checking" ? "checking its work" : "agent working",
          run ? [run.attempt ? `attempt ${run.attempt}` : null, run.age_seconds !== null && run.age_seconds !== undefined ? `${span(run.age_seconds)} in` : null, dollars(run.cost_usd), run.worker_id].filter(Boolean).join(" · ") : "",
        ),
        el(
          "p",
          { className: "fwb-agent-now" },
          call ? el("b", { textContent: call.tool }) : null,
          el("span", { textContent: call ? call.argument : lane === "checking" ? "its checks are running" : "no tool calls yet" }),
        ),
        runId
          ? el(
            "div",
            { className: "fwb-actions" },
            el("button", { type: "button", className: "slab", onclick: watch(runId) }, icon("activity"), "Watch the agent"),
            local(cur.key) ? el("button", { type: "button", className: "slab ghost", title: "Open the run viewer's message bar", onclick: watch(runId, { say: true }) }, icon("send"), "Message it") : null,
            local(cur.key) ? el("button", { type: "button", className: "slab ghost fwb-act-end", title: "Open the run viewer: Interrupt there asks before it stops the agent", onclick: watch(runId) }, icon("stop"), "Interrupt...") : null,
          )
          : null,
      );
    }
    if (lane === "needs") return needsBand(k, state, status, latest);
    if (lane === "done") {
      return el(
        "div",
        { className: "fwb-agent done" },
        bandHead(
          el("i", { className: "fw-dot idle", "aria-hidden": "true" }),
          state === "parked" ? "verified, landing parked" : "verified",
          latest ? `${latest.updated_at ? `${age(latest.updated_at)} ago · ` : ""}${h.runs.length === 1 && latest.attempts.length === 1 ? "first try" : `after ${plural(h.runs.length, "run")}`}` : "",
        ),
        latest ? el("div", { className: "fwb-actions" }, el("button", { type: "button", className: "slab ghost", onclick: watch(latest.run_id) }, icon("activity"), "Transcript")) : null,
      );
    }
    const waits = k.info ? k.info.prerequisites.filter((p) => !["verified", "parked"].includes(k.states.get(p) || "verified")) : [];
    const queued = k.info && k.info.queued_in;
    return el(
      "div",
      { className: "fwb-agent" },
      bandHead(
        el("i", { className: "fw-dot idle", "aria-hidden": "true" }),
        state === "blocked" ? "blocked" : waits.length ? "waiting" : "ready",
        queued ? `queued in ${queued.name}` : waits.length ? `until ${waits.join(", ")} verif${waits.length === 1 ? "ies" : "y"}` : "the next free worker claims it",
      ),
      state === "blocked" && k.info && k.info.blocked_on ? el("p", { className: "fwb-hint", textContent: `waits on ${k.info.blocked_on}` }) : null,
      el(
        "div",
        { className: "fwb-actions" },
        runButton(k, state, waits.length ? "Run it alone" : "Run now"),
        latest ? el("button", { type: "button", className: "slab ghost", onclick: watch(latest.run_id) }, icon("activity"), "Last run") : null,
      ),
      waits.length && local(cur.key) ? el("p", { className: "fwb-hint", textContent: `Run starts a worker on ${cur.id} only; it waits for its prerequisites to verify.` }) : null,
    );
  }

  function needsBand(k, state, status, latest) {
    const info = k.info;
    const runId = (latest && latest.run_id) || (info && info.run_id);
    let head = `${state} after ${latest ? plural(latest.attempts.length, "attempt") : "its run"}`;
    if (status === "interrupted") head = "interrupted: waits for you";
    if (state === "awaiting") head = "awaiting your approval";
    if (state === "parked") head = "landing parked";
    const tries = latest ? latest.attempts : [];
    const detail = state === "awaiting"
      ? el("p", { className: "fwb-gate-ask", textContent: (info && info.awaiting) || "manual approval" })
      : state === "parked" && info && info.stranded
        ? el("p", { className: "fwb-hint" }, el("b", { textContent: info.stranded.kind }), ` ${info.stranded.detail}`)
        : tries.length
          ? el("ol", { className: "fwb-tries" }, tries.map((a) => el(
            "li",
            { className: a.outcome === "succeeded" ? "ok" : a.outcome ? "fail" : "" },
            el("span", { className: "fwb-try-n", textContent: String(a.number) }),
            el("span", { className: "fwb-try-what" }, el("b", { textContent: outcomeWords(a.outcome) }), a.error ? el("span", { textContent: a.error }) : null),
            el("small", { textContent: dollars(a.cost_usd) }),
          )))
          : info && info.error ? el("p", { className: "fwb-hint", textContent: info.error }) : null;
    return el(
      "div",
      { className: `fwb-agent ${state}` },
      bandHead(el("i", { className: "fw-dot alert", "aria-hidden": "true" }), head, latest ? `${dollars(latest.cost_usd)}${latest.updated_at ? ` · ${age(latest.updated_at)} ago` : ""}` : ""),
      detail,
      state === "awaiting" ? gateBox(k, runId) : null,
      el(
        "div",
        { className: "fwb-actions" },
        runButton(k, state, state === "awaiting" ? "Run to apply" : "Run again", state === "awaiting"),
        runId ? el("button", { type: "button", className: "slab ghost", onclick: watch(runId) }, icon("activity"), "What happened") : null,
      ),
    );
  }

  // Approve, or reject with feedback, the run's parked manual gate.
  function gateBox(k, runId) {
    if (!runId || !local(cur.key)) return null;
    const gate = (k.panel && k.panel.intake && k.panel.intake.gates.find((g) => g.run_id === runId))
      || { run_id: runId, task_id: cur.id, instruction: (k.info && k.info.awaiting) || "" };
    const pending = flywheel.gates.pendingOf(gate);
    if (pending) {
      return el("p", {
        className: "ap-pending",
        role: "status",
        textContent: pending.command_id == null
          ? `Sending the ${pending.kind === "approve" ? "approval" : "rejection"}...`
          : `${pending.kind === "approve" ? "Approval" : "Rejection"} queued (command #${pending.command_id}); the next worker run on ${cur.id} applies it.`,
      });
    }
    const decide = async (approve) => {
      const feedback = approve ? "" : cur.feedback.trim();
      const mine = cur;
      const sent = flywheel.gates.send(cur.key, gate, approve, feedback);
      render();
      const done = await sent;
      if (cur !== mine) return;
      if (done && !approve) mine.feedback = "";
      html = null;
      render();
    };
    return el(
      "div",
      { className: "fwb-gate" },
      el("textarea", {
        rows: 2,
        maxLength: 4000,
        placeholder: "Feedback for the retry, if you reject (it goes into the agent's prompt)",
        "aria-label": `Feedback rejecting ${cur.id}`,
        "data-fw-input": "feedback",
        value: cur.feedback,
        oninput: (event) => {
          cur.feedback = event.target.value;
        },
      }),
      el(
        "div",
        { className: "fwb-actions" },
        el("button", { type: "button", className: "slab", "data-fw-action": "approve", title: `Approve gate of run ${runId}: the next worker run on ${cur.id} goes on`, onclick: () => decide(true) }, "Approve"),
        el("button", { type: "button", className: "slab danger", "data-fw-action": "reject", title: "Reject: the run retries with your feedback (one retry used)", onclick: () => decide(false) }, "Reject"),
      ),
    );
  }

  function goalSection(k, h) {
    const prereqs = k.info ? k.info.prerequisites : [];
    const facts = [
      [el("dt", { textContent: "plan" }), el("dd", { textContent: `${cur.plan}${k.plan && k.plan.archived ? " (archived)" : ""}` })],
      prereqs.length
        ? [el("dt", { textContent: "after" }), el("dd", {}, prereqs.map((q) => el(
          "span",
          { className: "fwb-req-fact" },
          el("span", { className: `fw-light ${k.states.get(q) || "verified"}`, textContent: k.states.get(q) || "done" }),
          el("button", { type: "button", className: "text-btn", textContent: q, title: `Open ${q}`, onclick: () => openSibling(q) }),
        )))]
        : null,
      k.info && k.info.flywheel_state ? [el("dt", { textContent: "state" }), el("dd", { textContent: k.info.flywheel_state.replace(/_/g, " ") })] : null,
    ];
    return el(
      "section",
      { className: "fw-sec fwb-goal-sec" },
      el("p", { className: "fwb-goal", textContent: h ? h.goal || "No task file defines this id now: its runs are below." : "Reading the task..." }),
      el("dl", { className: "fwb-facts" }, facts.filter(Boolean).flat()),
    );
  }

  // A prerequisite: its drawer, under the plan that holds it.
  function openSibling(id) {
    const panel = ctx.panelOf(cur.key);
    const plan = panel && (panel.plans || []).find((p) => (p.tasks || []).some((t) => t.id === id));
    if (plan) open(cur.key, id, plan.name);
  }

  // Each check's standing on the newest attempt that recorded receipts.
  function standing(h) {
    const out = new Map();
    for (const run of h.runs) {
      const a = [...run.attempts].reverse().find((x) => x.graders && x.graders.length);
      if (!a) continue;
      for (const g of a.graders) out.set(g.name, g.passed ? "pass" : "fail");
      break;
    }
    return out;
  }

  const KIND = { command: "command", rubric: "reviewer", manual: "manual", transcript: "transcript" };
  const MARK = { pass: ["✓", "passed on its latest receipt"], fail: ["✗", "failed on its latest receipt"], none: ["·", "no receipt yet"] };

  function checksSection(h) {
    if (!h) return null;
    const st = standing(h);
    const pass = h.checks.filter((c) => st.get(c.name) === "pass").length;
    const fail = h.checks.filter((c) => st.get(c.name) === "fail").length;
    const count = h.checks.length
      ? [plural(h.checks.length, "check"), fail ? `${fail} failed` : null, pass ? `${pass} passed` : null].filter(Boolean).join(" · ")
      : null;
    return el(
      "section",
      { className: "fw-sec" },
      el("h3", {}, "How it is checked", count ? el("span", { className: "fw-n", textContent: count }) : null),
      h.problem ? el("p", { className: "fw-problem", textContent: h.problem }) : null,
      h.checks.length
        ? el("ul", { className: "fwb-checks" }, h.checks.map((c) => {
          const s = st.get(c.name) || "none";
          return el(
            "li",
            { className: `fwb-check ${s}`, "data-fw-check": c.name, title: c.held_out ? "Held out: the agent never sees it; it runs on the committed result before it may land" : `${KIND[c.kind] || c.kind} check` },
            el("span", { className: "fwb-check-mark", title: MARK[s][1], "aria-label": MARK[s][1], textContent: MARK[s][0] }),
            el("b", { textContent: c.name }),
            el("small", { textContent: c.held_out ? "held out" : KIND[c.kind] || c.kind }),
          );
        }))
        : el("p", { className: "fw-empty", textContent: "No checks: done means only that the agent said so." }),
    );
  }

  function runsSection(h) {
    if (!h) return null;
    const cost = h.runs.reduce((n, r) => n + (r.cost_usd || 0), 0);
    return el(
      "section",
      { className: "fw-sec" },
      el("h3", {}, "Runs", el("span", { className: "fw-n", textContent: `${h.runs.length} · ${dollars(cost)}` })),
      h.runs.length
        ? el("ul", { className: "fw-list fwb-runs" }, h.runs.map(runRow))
        : el("p", { className: "fw-empty", textContent: "Not run yet." }),
    );
  }

  function runRow(r) {
    const live = r.status === "running" || r.status === "validating";
    const when = live && r.started_at ? `${age(r.started_at)} running` : r.updated_at ? `${age(r.updated_at)} ago` : "";
    const lastError = r.attempts.length ? r.attempts[r.attempts.length - 1].error : "";
    return el(
      "li",
      {
        className: `fw-run fwb-run${live ? " live" : ""}`,
        "data-fw-run": r.run_id,
        title: `Open run ${r.run_id} in the run viewer`,
        onclick: () => flywheel.openRun(cur.key, r.run_id, cur.id),
      },
      el(
        "div",
        { className: "fw-run-line" },
        el("span", { className: `fw-status ${r.status}`, textContent: runLabel(r.status) }),
        el("b", { textContent: r.run_id }),
        el("span", { className: "fw-run-when", textContent: when }),
        el("button", { type: "button", className: "text-btn fw-out", textContent: "output", "aria-label": `Open run ${r.run_id}` }),
      ),
      el("div", { className: "fw-run-sub" }, el("span", { className: "fw-run-cost", textContent: `${plural(r.attempts.length, "attempt")} · ${tokens(r.tokens)} tok · ${dollars(r.cost_usd)}` })),
      r.attempts.length
        ? el("ol", { className: "fwb-attempts" }, r.attempts.map((a) => el(
          "li",
          { className: a.outcome === "succeeded" ? "ok" : a.outcome ? "fail" : "" },
          el("b", { textContent: `attempt ${a.number}` }),
          el("span", { className: "fwb-att-outcome", textContent: outcomeWords(a.outcome) }),
          a.error ? el("span", { className: "fwb-att-error", textContent: a.error }) : null,
        )))
        : null,
      r.error && r.error !== lastError ? el("p", { className: "fw-run-error", textContent: r.error }) : null,
    );
  }

  // --- New task --------------------------------------------------------------

  const dialog = el("dialog", { id: "dlg-fwb-new", className: "sheet fwb-new", "aria-label": "New task" });
  dialog.addEventListener("click", (event) => {
    if (event.target.closest("[data-close]") || event.target === dialog) dialog.close("cancel");
  });
  dialog.addEventListener("close", () => {
    form = null;
    dialog.replaceChildren();
  });
  document.body.append(dialog);

  let form = null;

  const field = (label, hint, ...children) =>
    el("label", { className: "field" }, el("span", { className: "field-label" }, label, hint ? el("span", { className: "field-hint", textContent: ` ${hint}` }) : null), ...children);

  // The board's projects new tasks can go to: this hub's adopted folders.
  const targets = () => ctx.projects().filter((p) => local(p.key));

  // `defaults`: { key, phase } (the open plan's).
  function openNew(defaults = {}) {
    const projects = targets();
    if (!projects.length) return;
    const start = projects.find((p) => p.key === defaults.key) || projects[0];
    const project = el("select", { "aria-label": "Project" }, projects.map((p) => el("option", { value: p.key, textContent: `${p.label} (${p.path})` })));
    project.value = start.key;
    const goal = el("textarea", { rows: 3, maxLength: 600, placeholder: "The end state, as something a check could prove", "data-fw-input": "goal" });
    const id = el("input", { type: "text", maxLength: 100, spellcheck: false, autocomplete: "off", placeholder: "kebab-case, unique in the project", "data-fw-input": "id" });
    const phases = el("datalist", { id: "fwb-new-phases" });
    const phase = el("input", { type: "text", maxLength: 100, spellcheck: false, autocomplete: "off", placeholder: "an active phase, or a new kebab-case name", "data-fw-input": "phase" });
    phase.setAttribute("list", "fwb-new-phases");
    const prereqs = el("input", { type: "text", spellcheck: false, autocomplete: "off", placeholder: "task ids, comma-separated", "data-fw-input": "prerequisites" });
    const picks = el("div", { className: "fwb-reqs" });
    const checks = el("textarea", { rows: 3, spellcheck: false, className: "fwb-mono", placeholder: "one command per line, run from the worktree root; all must exit 0", "data-fw-input": "checks" });
    const error = el("p", { className: "fwb-new-error", role: "alert", "data-fw-error": "" });
    const preview = el("aside", { className: "fwb-new-preview", "aria-live": "polite" });
    const submit = el("button", { type: "submit", className: "slab", "data-fw-action": "new-task-submit" }, "Review");
    const actions = el("span", { className: "fwb-new-actions" }, submit);
    const node = el(
      "form",
      { method: "dialog", novalidate: true, "data-fw-form": "new-task" },
      el("header", { className: "sheet-head" }, el("span", { className: "sheet-kicker", textContent: "flywheel · add to the queue" }), el("h2", { textContent: "New task" })),
      el(
        "div",
        { className: "fwb-new-grid" },
        el(
          "div",
          { className: "fwb-new-fields" },
          projects.length > 1 ? field("Project", "", project) : null,
          field("Goal", "the end state a check could prove", goal),
          el("div", { className: "fo-two" }, field("Id", "", id), field("Phase", "tasks/active/<phase>/", phase, phases)),
          field("After", "it waits until each of these verifies", prereqs, picks),
          field("Checks", "one command per line", checks),
          error,
        ),
        preview,
      ),
      el("footer", { className: "sheet-foot" }, el("button", { type: "button", className: "slab ghost", "data-close": true, textContent: "Cancel" }), actions),
    );
    form = { project, goal, id, phase, phases, prereqs, picks, checks, error, preview, actions, submit, plan: null, seq: 0, idTouched: false };
    goal.addEventListener("input", () => {
      if (!form.idTouched) id.value = goal.value.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").split("-").slice(0, 5).join("-");
      edited();
    });
    id.addEventListener("input", () => {
      form.idTouched = id.value.trim() !== "";
      edited();
    });
    phase.addEventListener("input", () => {
      followPhase();
      edited();
    });
    project.addEventListener("change", () => {
      fillPhases();
      edited();
    });
    for (const input of [prereqs, checks]) input.addEventListener("input", edited);
    node.addEventListener("submit", (event) => {
      event.preventDefault();
      plan();
    });
    const panel = ctx.panelOf(start.key);
    const active = panel ? panel.phases.map((p) => p.name) : [];
    if (defaults.phase && active.includes(defaults.phase)) phase.value = defaults.phase;
    dialog.replaceChildren(node);
    fillPhases();
    showPreview();
    if (!dialog.open) dialog.showModal();
    goal.focus();
  }

  const keyOfForm = () => form.project.value;
  const listOf = (text) => text.split(/[\s,]+/).map((x) => x.trim()).filter(Boolean);
  const commandsOf = (text) => text.split("\n").map((x) => x.trim()).filter(Boolean);

  // The chosen project's active phases (the datalist) and its tasks to wait for.
  function fillPhases() {
    const panel = ctx.panelOf(keyOfForm());
    if (!panel) ctx.loadPanel(keyOfForm()).then(() => form && fillPhases(), () => {});
    form.phases.replaceChildren(...(panel ? panel.phases.map((p) => el("option", { value: p.name })) : []));
    renderPicks();
  }

  // A phase only another project has: the project follows it.
  function followPhase() {
    const name = form.phase.value.trim();
    if (!name) return;
    const has = (key) => {
      const panel = ctx.panelOf(key);
      return !!panel && panel.phases.some((p) => p.name === name);
    };
    if (has(keyOfForm())) return;
    const others = targets().filter((p) => has(p.key));
    if (others.length === 1) {
      form.project.value = others[0].key;
      fillPhases();
    }
  }

  // The phase's tasks as toggles for After (unfinished ones first).
  function renderPicks() {
    const panel = ctx.panelOf(keyOfForm());
    const name = form.phase.value.trim();
    const ph = panel && panel.phases.find((p) => p.name === name);
    const picked = new Set(listOf(form.prereqs.value));
    const tasks = ph ? [...ph.tasks].sort((a, b) => (a.state === "verified") - (b.state === "verified")) : [];
    form.picks.replaceChildren(...tasks.map((t) => el(
      "button",
      {
        type: "button",
        className: `fwb-req${picked.has(t.id) ? " on" : ""}${t.state === "verified" ? " fwb-req-done" : ""}`,
        "aria-pressed": String(picked.has(t.id)),
        title: picked.has(t.id) ? `Drop ${t.id}` : `Wait for ${t.id}`,
        onclick: () => {
          const now = listOf(form.prereqs.value);
          form.prereqs.value = (now.includes(t.id) ? now.filter((x) => x !== t.id) : [...now, t.id]).join(", ");
          edited();
        },
      },
      el("span", { className: `fw-light ${t.state}`, textContent: t.state }),
      t.id,
    )));
  }

  // Any edit: the plan shown is no longer what would be written.
  function edited() {
    if (!form) return;
    form.seq++;
    form.plan = null;
    form.error.textContent = "";
    form.actions.replaceChildren(form.submit);
    form.submit.textContent = "Review";
    renderPicks();
    showPreview();
  }

  function request() {
    return {
      folder: flywheel.pathOf(keyOfForm()),
      task_id: form.id.value.trim(),
      goal: form.goal.value.trim(),
      phase: form.phase.value.trim() || undefined,
      prerequisites: listOf(form.prereqs.value),
      checks: commandsOf(form.checks.value).map((run) => ({ run })),
    };
  }

  // Where it lands, when it is picked up, what done means: from the
  // fields, then the hub's plan (the exact file) once reviewed.
  function showPreview() {
    const r = request();
    const panel = ctx.panelOf(keyOfForm());
    const states = new Map();
    for (const p of (panel && panel.plans) || []) for (const t of p.tasks || []) states.set(t.id, t.state);
    const waits = r.prerequisites.filter((q) => !["verified", "parked"].includes(states.get(q)));
    const plan = form.plan;
    form.preview.replaceChildren(
      el(
        "section",
        {},
        el("h5", { textContent: "picked up" }),
        el("p", { className: "fwb-new-answer", textContent: waits.length ? `After ${plural(waits.length, "task")}` : "Once queued" }),
        waits.length ? el("ul", { className: "fwb-new-waits" }, waits.map((q) => el("li", {}, el("span", { className: `fw-light ${states.get(q) || "pending"}`, textContent: states.get(q) || "?" }), el("span", { textContent: q })))) : null,
      ),
      el(
        "section",
        {},
        el("h5", { textContent: "done when" }),
        el("p", { className: "fwb-new-answer", textContent: r.checks.length ? (r.checks.length === 1 ? "Its command exits 0" : `All ${r.checks.length} commands exit 0`) : "Name a check" }),
        r.checks.length ? el("ul", { className: "fwb-new-cmds" }, r.checks.map((c) => el("li", {}, el("code", { textContent: c.run })))) : el("p", { className: "fwb-new-dim", textContent: "The loop verifies a task by its commands; add one whose exit code proves the goal." }),
      ),
      el(
        "section",
        {},
        el("h5", { textContent: "lands at" }),
        el("p", {}, el("code", { textContent: plan ? plan.relative : `tasks/active/${r.phase || "<phase>"}/${r.task_id || "<id>"}.json` })),
        el("p", { className: "fwb-new-dim", textContent: plan ? (plan.new_phase ? `A new phase, ${plan.phase}.` : `Phase ${plan.phase}.`) : "In the project dir under the flywheel home; nothing is written into the repo." }),
        plan ? el("details", { className: "fw-more" }, el("summary", { textContent: "the file, word for word" }), el("pre", { className: "fo-cmds", textContent: plan.contents })) : null,
      ),
    );
  }

  // What the hub would refuse, named before asking it (it still decides):
  // the fields, and ids against the project's every plan (archived too).
  function problems(r) {
    const out = [];
    const panel = ctx.panelOf(keyOfForm());
    const known = new Map();
    for (const p of (panel && panel.plans) || []) for (const t of p.tasks || []) known.set(t.id, p.name);
    if (!r.goal) out.push("needs a goal: one sentence saying what the change does");
    if (!r.task_id) out.push("needs an id");
    else if (!SEGMENT.test(r.task_id) || r.task_id.includes("..")) out.push(`id ${r.task_id} must be 1-100 of A-Z a-z 0-9 . _ -, starting with a letter or digit`);
    else if (known.has(r.task_id)) out.push(`id ${r.task_id} is already a task in ${known.get(r.task_id)}`);
    const active = panel ? panel.phases.map((p) => p.name) : null;
    if (r.phase && active && !active.includes(r.phase) && !KEBAB.test(r.phase)) out.push(`a new phase name must be kebab-case, not ${r.phase}`);
    for (const q of r.prerequisites) {
      if (q === r.task_id) out.push(`${q} cannot wait for itself`);
      else if (panel && !known.has(q)) out.push(`unknown prerequisite ${q}: no task by that id in this project`);
    }
    if (!r.checks.length) out.push("name at least one command check: the loop verifies a task by them");
    return out;
  }
  const SEGMENT = /^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/;
  const KEBAB = /^[a-z0-9]+(-[a-z0-9]+)*$/;

  // Review: the hub's dry run (nothing written), then the confirm.
  async function plan() {
    if (!form) return;
    const mine = form;
    const seq = ++mine.seq;
    mine.error.textContent = "";
    const refused = problems(request());
    if (refused.length) {
      mine.error.textContent = refused.join("; ");
      return;
    }
    mine.submit.disabled = true;
    try {
      const answer = await flywheel.post("/api/flywheel/tasks", { ...request(), dry_run: true });
      if (form !== mine || seq !== mine.seq) return;
      if (!answer.ok) throw new Error((answer.data && answer.data.error) || `HTTP ${answer.status}`);
      mine.plan = { ...answer.data.plan, fingerprint: answer.data.fingerprint };
      mine.submit.textContent = "Review again";
      mine.actions.replaceChildren(
        mine.submit,
        el("button", { type: "button", className: "slab", "data-fw-action": "new-task-confirm", onclick: write }, icon("plus"), "Add to queue"),
      );
      showPreview();
      mine.actions.querySelector("[data-fw-action=new-task-confirm]").focus();
    } catch (error) {
      if (form === mine && seq === mine.seq) mine.error.textContent = error.message;
    } finally {
      mine.submit.disabled = false;
    }
  }

  async function write(event) {
    const mine = form;
    if (!mine || !mine.plan) return;
    const button = event.currentTarget;
    if (button.disabled) return;
    button.disabled = true;
    const key = keyOfForm();
    let answer;
    try {
      answer = await flywheel.post("/api/flywheel/tasks", { ...request(), confirm: mine.plan.fingerprint });
    } catch (error) {
      answer = { ok: false, data: { error: error.message } };
    }
    if (form !== mine) return;
    if (!answer.ok) {
      button.disabled = false;
      mine.error.textContent = (answer.data && answer.data.error) || `HTTP ${answer.status}`;
      return;
    }
    const w = answer.data.written;
    dialog.close();
    toast(el("span", {}, "Added ", el("b", { textContent: w.task_id }), ` to ${w.phase}: ${w.relative}`), { timeout: 5000 });
    ctx.loadPanel(key);
  }

  return {
    node,
    open,
    close,
    refresh,
    render,
    openNew,
    isOpen: () => !!cur,
    openId: () => (cur ? cur.id : null),
    openKey: () => (cur ? cur.key : null),
  };
};

// flywheel hub: the flywheel board, a layout tab kind (app-stage.js mounts
// it into the tab's stage). Portfolio: every plan (active and archived) of
// every adopted folder in the tab's workspace, grouped by project, the
// plans that need someone first. A plan opens into five lanes of its
// tasks -- Needs you, Queued, Running, Checking, Done -- kept live. Data:
// flywheel.js's summaries (kept current by the {"type":"flywheel"}
// pushes) and, while a plan is open, its folder's panel (GET
// /api/flywheel/panel, re-read on the folder's push and every few
// seconds). Hooks for checks: the data-fw-* attributes, raw values beside
// the formatted ones.
"use strict";

window.FwHubBoard = function (ctx) {
  const { el, icon, state, workspaceById, flywheel, span } = ctx;
  const { dollars, plural, pathTail } = window.FwHubFmt;

  // An open plan re-reads its panel this often (pushes come sooner).
  const POLL_MS = 4000;

  const LANES = [
    { id: "needs", title: "Needs you", rule: "failed, parked at landing, or awaiting approval", empty: "Nothing is stopped." },
    { id: "queued", title: "Queued", rule: "not run yet, or blocked", empty: "Nothing waits." },
    { id: "running", title: "Running", rule: "an agent is on it", empty: "No agent is working." },
    { id: "checking", title: "Checking", rule: "the agent said done; its checks run", empty: "Nothing is being checked." },
    { id: "done", title: "Done", rule: "its checks passed", empty: "Nothing verified yet." },
  ];
  // A task's lane, from its light and its latest run's lifecycle status:
  // the one mapping (the task drawer reads it too).
  const LANE_OF = {
    failed: "needs",
    parked: "needs",
    awaiting: "needs",
    pending: "queued",
    blocked: "queued",
    verified: "done",
  };
  // A run that has not started yet (pending, ready) still waits in Queued.
  const RUN_LANE = { validating: "checking", pending: "queued", ready: "queued" };
  // An interrupted run waits for a person to release it, so it needs you;
  // the panel's light calls it blocked, the same as an unmet prerequisite.
  const laneOf = (light, status) => (status === "interrupted" ? "needs" : light === "running" ? RUN_LANE[status] || "running" : LANE_OF[light] || "queued");

  const views = new Map(); // board tab id -> { plan: { key, name } | null }
  const panels = new Map(); // folder key -> { panel, error, seq, loading }
  let mounted = null; // { node, root, html }

  const viewOf = (id) => {
    if (!views.has(id)) views.set(id, { plan: null });
    return views.get(id);
  };
  const shown = () => (mounted && mounted.root.isConnected ? mounted : null);
  const workspace = () => workspaceById(state.view) || null;

  // --- mount and paint -------------------------------------------------------

  function mount(node) {
    const root = el("section", {
      className: "board-stage fwb",
      "data-fw-board": node.folder,
      "aria-label": `Flywheel board of ${node.folder}`,
      onkeydown: (event) => {
        const v = viewOf(node.id);
        if (event.key === "Escape" && v.plan && !document.querySelector("dialog[open]")) {
          event.stopPropagation();
          closePlan();
        }
      },
    });
    mounted = { node, root, html: null };
    // Painted before the stage attaches it.
    paint(mounted);
    const v = viewOf(node.id);
    if (v.plan) loadPanel(v.plan.key);
    return root;
  }

  // Rebuild the shown board; a rebuild that changes nothing keeps the DOM
  // (focus, hover, selection) as it is. `m`: the mounted board (default:
  // the one shown).
  function paint(m = shown()) {
    if (!m) return;
    const w = workspace();
    const v = viewOf(m.node.id);
    const top = head(w, v);
    const box = el("div", { className: "fwb-body" }, ...(v.plan ? planView(w, v.plan) : portfolio(w)));
    const html = top.outerHTML + box.outerHTML;
    if (html === m.html) return;
    m.html = html;
    const active = document.activeElement;
    const hook = active && m.root.contains(active) ? hookOf(active) : null;
    const scroll = m.root.scrollTop;
    m.root.replaceChildren(top, box);
    m.root.scrollTop = scroll;
    if (hook) {
      const again = m.root.querySelector(`[${hook.name}="${CSS.escape(hook.value)}"]`);
      if (again) again.focus({ preventScroll: true });
    }
  }

  // The data-fw-* attribute naming a focused element, to refocus its
  // successor after a rebuild.
  function hookOf(node) {
    for (const attr of node.attributes) {
      if (/^data-fw-(plan|task|action)$/.test(attr.name)) return { name: attr.name, value: attr.value };
    }
    return null;
  }

  // --- data ------------------------------------------------------------------

  // The workspace's adopted folders (with their summaries) and the rest.
  function projects(w) {
    const out = [];
    const others = [];
    for (const f of (w && w.folders) || []) {
      const key = flywheel.keyOf(w.machine || null, f.path);
      const s = flywheel.summary(key);
      if (!s) {
        others.push(f);
        continue;
      }
      const plans = [...(s.plans || [])].sort(byAttention);
      const sum = (k) => plans.reduce((n, p) => n + (p[k] || 0), 0);
      out.push({
        key,
        path: f.path,
        label: flywheel.labelFor(key),
        summary: s,
        plans,
        needs: sum("needs"),
        running: sum("running"),
        verified: sum("verified"),
        total: sum("total"),
        rate: sum("verified_24h"),
        spend: sum("spend_usd"),
      });
    }
    out.sort((a, b) => (b.needs > 0) - (a.needs > 0) || a.label.localeCompare(b.label));
    return { out, others };
  }

  // What needs a person first, then by name.
  const byAttention = (a, b) => (b.needs > 0) - (a.needs > 0) || a.name.localeCompare(b.name);

  async function loadPanel(key) {
    const entry = panels.get(key) || { panel: null, error: "", seq: 0, loading: false };
    panels.set(key, entry);
    const seq = ++entry.seq;
    entry.loading = true;
    try {
      const panel = await flywheel.readPanel(key);
      if (seq !== entry.seq) return;
      entry.panel = panel;
      entry.error = "";
    } catch (error) {
      if (seq !== entry.seq) return;
      entry.error = error.message;
    }
    entry.loading = false;
    paint();
  }

  const openKey = () => {
    const m = shown();
    const v = m ? viewOf(m.node.id) : null;
    return v && v.plan ? v.plan.key : null;
  };

  setInterval(() => {
    const key = openKey();
    if (!key || document.visibilityState !== "visible") return;
    const entry = panels.get(key);
    if (!entry || !entry.loading) loadPanel(key);
  }, POLL_MS);

  // A folder's push (app-events.js): its open plan re-reads (a reset: any
  // open plan on that machine).
  function onFlywheel(message, machine) {
    const key = openKey();
    if (!key) return;
    const folder = message.type === "flywheel_reset" ? flywheel.pathOf(key) : message.folder;
    if (flywheel.keyOf(machine || null, folder) === key) loadPanel(key);
  }

  function openPlan(key, name) {
    const m = shown();
    if (!m) return;
    viewOf(m.node.id).plan = { key, name };
    m.root.scrollTop = 0;
    paint();
    loadPanel(key);
  }

  function closePlan() {
    const m = shown();
    if (!m) return;
    const v = viewOf(m.node.id);
    const was = v.plan;
    v.plan = null;
    paint();
    const row = was && m.root.querySelector(`[data-fw-project="${CSS.escape(flywheel.pathOf(was.key))}"] [data-fw-plan="${CSS.escape(was.name)}"]`);
    if (row) row.focus();
  }

  // --- shared pieces ---------------------------------------------------------

  // Verified of total on a quiet track: one tone (the other states live
  // in the plan's lanes).
  function progress(verified, total) {
    const pct = total ? Math.round((100 * verified) / total) : 0;
    return el(
      "span",
      { className: "fwb-prog", title: `${verified} of ${plural(total, "task")} verified` },
      el("span", { className: "fwb-bar" }, el("i", { style: `width:${pct}%` })),
      el("small", { textContent: `${verified}/${total}` }),
    );
  }

  const num = (cls, n, text, title) => el("span", { className: `fwb-n ${cls}${n ? " on" : ""}`, title: title || null, textContent: n ? text || String(n) : "-" });

  // --- head ------------------------------------------------------------------

  function head(w, v) {
    const crumbs = [];
    if (v.plan) {
      crumbs.push(
        el("button", { type: "button", className: "text-btn fwb-back", title: "Back to the portfolio (Esc)", onclick: closePlan }, icon("chev"), "Portfolio"),
        el("span", { className: "fwb-crumb", textContent: flywheel.labelFor(v.plan.key) }),
        el("strong", { textContent: v.plan.name }),
      );
    } else {
      crumbs.push(
        el("strong", { textContent: "Portfolio" }),
        el("span", { className: "fwb-crumb", textContent: (w && w.name) || "" }),
      );
    }
    return el("header", { className: "board-head fwb-head" }, el("nav", { className: "fwb-crumbs", "aria-label": "Board" }, crumbs), pulse(w, v));
  }

  // The head's answer to "what needs me, what is moving": the open plan's
  // numbers, else the whole portfolio's.
  function pulse(w, v) {
    let p;
    if (v.plan) {
      const entry = panels.get(v.plan.key);
      p = entry && entry.panel ? (entry.panel.plans || []).find((x) => x.name === v.plan.name) : null;
      if (!p) return null;
      p = { needs: p.needs, running: p.running, rate: p.verified_24h, spend: p.spend_usd, verified: p.verified, total: p.total };
    } else {
      const { out } = projects(w);
      if (!out.length) return null;
      const sum = (k) => out.reduce((n, x) => n + x[k], 0);
      p = { needs: sum("needs"), running: sum("running"), rate: sum("rate"), spend: sum("spend"), verified: sum("verified"), total: sum("total") };
    }
    const item = (cls, n, label) => el("span", { className: `fwb-pulse-item ${cls}${n ? " on" : ""}` }, el("b", { textContent: String(n) }), label);
    return el(
      "div",
      { className: "fwb-pulse" },
      item("needs", p.needs, p.needs === 1 ? "needs you" : "need you"),
      item("moving", p.running, "running"),
      item("rate", p.rate, "verified in 24h"),
      el("span", { className: "fwb-pulse-item" }, el("b", { textContent: `${p.verified}/${p.total}` }), "verified"),
      el("span", { className: "fwb-pulse-item" }, el("b", { textContent: dollars(p.spend) }), "spent"),
    );
  }

  // --- portfolio -------------------------------------------------------------

  function portfolio(w) {
    if (!flywheel.loaded()) return [el("p", { className: "fwb-quiet", textContent: "Reading the workspace's flywheel projects..." })];
    const { out, others } = projects(w);
    if (!out.length) return [emptyState(w)];
    return [
      el(
        "div",
        { className: "fwb-cols", "aria-hidden": "true" },
        el("span"),
        el("span", { textContent: "plan" }),
        el("span", { textContent: "verified" }),
        el("span", { textContent: "agents" }),
        el("span", { textContent: "24h" }),
        el("span", { textContent: "needs you" }),
        el("span", { textContent: "spend" }),
        el("span"),
      ),
      ...out.map(projectBlock),
      others.length
        ? el(
          "p",
          { className: "fwb-quiet fwb-others" },
          `Not adopted: ${others.map((f) => f.name || f.path.split("/").pop()).join(", ")}. Run `,
          el("code", { textContent: "fw init" }),
          " in a folder's repo to add it.",
        )
        : null,
    ];
  }

  function emptyState(w) {
    const folders = (w && w.folders) || [];
    return el(
      "div",
      { className: "fwb-empty", "data-fw-empty": "" },
      el("h3", { textContent: "No flywheel project here yet" }),
      el(
        "p",
        {},
        folders.length
          ? `None of this workspace's ${plural(folders.length, "folder")} is adopted by flywheel. `
          : "This workspace has no folders. Add a repo folder, then adopt it. ",
        "To adopt a repo, run ",
        el("code", { textContent: "fw init" }),
        " in it; its plans show here once it has tasks.",
      ),
      flywheel.enabled() ? null : el("p", { className: "fwb-quiet", textContent: "This hub reads no flywheel data (it runs without the flywheel provider)." }),
    );
  }

  function projectBlock(p) {
    const s = p.summary;
    return el(
      "section",
      { className: `fwb-proj${p.needs ? " needs" : p.running ? " moving" : ""}`, "data-fw-project": flywheel.pathOf(p.key) },
      el(
        "div",
        { className: "fwb-pf-row fwb-pf-proj", title: `${p.path}\n${plural(p.plans.length, "plan")}` },
        el("span", { className: "fwb-mark" }, el("i", { className: `fw-dot ${p.needs ? "alert" : p.running ? "live" : "idle"}`, "aria-hidden": "true" })),
        el(
          "span",
          { className: "fwb-pf-title" },
          el("b", { textContent: p.label }),
          el("small", { className: "path-tail", textContent: pathTail(p.path) }),
        ),
        progress(p.verified, p.total),
        num("fwb-agents", p.running, null, `${plural(p.running, "task")} running`),
        num("fwb-rate", p.rate, `${p.rate}/d`, `${plural(p.rate, "task")} verified in the last 24h`),
        num("fwb-needs", p.needs, null, `${plural(p.needs, "task")} failed, parked, or awaiting approval`),
        el("span", { className: "fwb-n fwb-spend", textContent: dollars(p.spend) }),
        el("span"),
      ),
      s.problem ? el("p", { className: "fwb-problem", role: "alert", "data-fw-problem": "", textContent: s.problem }) : null,
      p.plans.length
        ? el("ul", { className: "fwb-pf-rows" }, p.plans.map((plan) => planRow(p.key, plan)))
        : el("p", { className: "fwb-quiet", textContent: "No plans yet: no phase under its tasks/active or tasks/archive." }),
    );
  }

  function planRow(key, p) {
    const tone = p.needs ? " needs" : p.running ? " moving" : p.archived ? " archived" : "";
    return el(
      "li",
      {},
      el(
        "button",
        {
          type: "button",
          className: `fwb-pf-row fwb-pf-plan${tone}`,
          "data-fw-plan": p.name,
          "data-fw-verified": String(p.verified),
          "data-fw-total": String(p.total),
          "data-fw-spend-cents": String(Math.round((p.spend_usd || 0) * 100)),
          "data-fw-verified-24h": String(p.verified_24h),
          "data-fw-needs": String(p.needs),
          title: `${p.name}${p.archived ? " (archived)" : ""}: ${p.verified}/${p.total} verified, ${p.running} running, ${p.needs} need you, ${p.verified_24h} verified in 24h, ${dollars(p.spend_usd)} spent. Open its lanes.`,
          onclick: () => openPlan(key, p.name),
        },
        el("span", { className: "fwb-mark" }, p.running && !p.needs ? el("i", { className: "fw-dot live", "aria-hidden": "true" }) : null),
        el(
          "span",
          { className: "fwb-pf-title" },
          el("b", { textContent: p.name }),
          p.archived ? el("small", { textContent: "archived" }) : null,
        ),
        progress(p.verified, p.total),
        num("fwb-agents", p.running),
        num("fwb-rate", p.verified_24h, `${p.verified_24h}/d`),
        num("fwb-needs", p.needs),
        el("span", { className: "fwb-n fwb-spend", textContent: p.spend_usd ? dollars(p.spend_usd) : "-" }),
        icon("chev"),
      ),
    );
  }

  // --- plan: five lanes ------------------------------------------------------

  // The plan's tasks with what the panel knows of each: its listing (an
  // active task's error, gate, prerequisites) and its live run.
  function planTasks(panel, plan) {
    const listed = new Map(panel.phases.flatMap((ph) => ph.tasks).map((t) => [t.id, t]));
    const live = new Map(panel.live.map((r) => [r.task_id, r]));
    // A hub predating the plans' task lists: the phase's listed tasks.
    const own = plan.tasks || ((panel.phases.find((ph) => ph.name === plan.name) || { tasks: [] }).tasks.map((t) => ({ id: t.id, state: t.state, status: t.run_status })));
    return own.map((t) => {
      const run = live.get(t.id) || null;
      return { id: t.id, state: t.state, info: listed.get(t.id) || null, live: run, lane: laneOf(t.state, t.status || (run && run.status)) };
    });
  }

  function planView(w, open) {
    const entry = panels.get(open.key);
    const panel = entry && entry.panel;
    if (!panel) {
      return [el("p", { className: entry && entry.error ? "fwb-problem" : "fwb-quiet", textContent: (entry && entry.error) || "Reading the plan..." })];
    }
    const plan = (panel.plans || []).find((p) => p.name === open.name);
    if (!plan) {
      return [el("p", { className: "fwb-quiet", textContent: `${flywheel.labelFor(open.key)} has no plan named ${open.name} now.` })];
    }
    const tasks = planTasks(panel, plan);
    const done = new Set(tasks.filter((t) => t.state === "verified" || t.state === "parked").map((t) => t.id));
    const lanes = LANES.map((lane) => {
      const list = tasks.filter((t) => t.lane === lane.id);
      if (lane.id === "queued") {
        // Ready before waiting on a prerequisite, then by id.
        const waits = (t) => (t.info ? t.info.prerequisites.some((p) => !done.has(p)) : false);
        list.sort((a, b) => waits(a) - waits(b) || a.id.localeCompare(b.id));
      }
      const rows = lane.id === "queued" || lane.id === "done";
      return el(
        "section",
        { className: `fwb-lane ${lane.id}${list.length ? "" : " is-empty"}`, "data-fw-lane": lane.id, "aria-label": lane.title },
        el(
          "header",
          { className: "fwb-lane-head", title: `${lane.title}: ${lane.rule}` },
          el("h4", { textContent: lane.title }),
          el("span", { className: `fwb-lane-count${list.length ? "" : " zero"}`, textContent: String(list.length) }),
        ),
        list.length
          ? rows
            ? el("ul", { className: "fwb-rows" }, list.map((t) => taskRow(open.key, t, done)))
            : el("div", { className: "fwb-cards" }, list.map((t) => taskCard(open.key, t)))
          : el("p", { className: "fwb-lane-empty", textContent: lane.empty }),
      );
    });
    return [
      entry.error ? el("p", { className: "fwb-problem", role: "alert", textContent: `Not re-read: ${entry.error}` }) : null,
      panel.problem ? el("p", { className: "fwb-problem", "data-fw-problem": "", textContent: panel.problem }) : null,
      el(
        "section",
        { className: "fwb-plan", "data-fw-plan-view": plan.name },
        el("div", { className: "fwb-lanes" }, lanes),
      ),
    ].filter(Boolean);
  }

  // A task's latest run's output (the run viewer), when it has one.
  function activate(key, t) {
    const run = (t.live && t.live.run_id) || (t.info && t.info.run_id);
    if (run) flywheel.openRun(key, run, t.id);
  }

  // Why a needs-you task stopped, from the panel's listing.
  function stopped(t) {
    const info = t.info;
    if (t.state === "awaiting") return { head: "awaiting approval", detail: (info && info.awaiting) || "" };
    if (t.state === "parked") {
      const park = info && info.stranded;
      return { head: park ? `landing parked: ${park.kind}` : "landing parked", detail: park ? park.detail : "" };
    }
    return { head: info && info.run_status ? `latest run ${info.run_status.replace(/_/g, " ")}` : "failed", detail: (info && info.error) || "" };
  }

  function taskCard(key, t) {
    const title = `${t.id}: ${t.state}${t.info && t.info.run_id ? `. Open run ${t.info.run_id}'s output.` : ""}`;
    if (t.lane === "needs") {
      const why = stopped(t);
      return el(
        "button",
        { type: "button", className: `fwb-card fwb-card-needs ${t.state}`, "data-fw-task": t.id, title, onclick: () => activate(key, t) },
        el("span", { className: "fwb-card-top" }, el("span", { className: "fwb-card-id", textContent: t.id })),
        el("span", { className: "fwb-card-why" }, el("span", { className: `fw-light ${t.state}`, textContent: t.state }), el("b", { textContent: why.head })),
        why.detail ? el("span", { className: "fwb-card-error", title: why.detail, textContent: why.detail }) : null,
      );
    }
    const run = t.live;
    const checking = t.lane === "checking";
    const call = run && run.last_tool;
    const now = el(
      "span",
      { className: `fwb-card-now${call ? "" : " idle"}`, "data-fw-last-tool": "", title: call ? `${call.tool} ${call.argument}` : null },
      call ? el("span", { className: "fwb-card-tool", textContent: call.tool }) : null,
      call ? " " : null,
      el("span", { className: "fwb-card-what", textContent: call ? call.argument : checking ? "checks running" : "no tool calls yet" }),
    );
    const meta = run
      ? [run.attempt ? `attempt ${run.attempt}` : null, run.iteration ? `iter ${run.iteration}` : null, dollars(run.cost_usd), run.worker_id].filter(Boolean).join(" · ")
      : "";
    return el(
      "button",
      { type: "button", className: `fwb-card fwb-card-live${checking ? " checking" : ""}`, "data-fw-task": t.id, title, onclick: () => activate(key, t) },
      el(
        "span",
        { className: "fwb-card-top" },
        el("i", { className: `fw-dot ${checking ? "idle" : "live"}`, "aria-hidden": "true" }),
        el("span", { className: "fwb-card-id", textContent: t.id }),
        run && run.age_seconds !== null && run.age_seconds !== undefined ? el("small", { className: "fwb-card-age", textContent: span(run.age_seconds) }) : null,
      ),
      now,
      meta ? el("span", { className: "fwb-card-meta", textContent: meta }) : null,
    );
  }

  function taskRow(key, t, done) {
    const waits = t.lane === "queued" && t.info ? t.info.prerequisites.filter((p) => !done.has(p)) : [];
    const right = t.state === "blocked"
      ? el("small", { textContent: "blocked", title: (t.info && t.info.blocked_on) || "interrupted" })
      : waits.length
        ? el("small", { className: "fwb-row-after", title: `waits for ${waits.join(", ")}`, textContent: `after ${waits[0]}${waits.length > 1 ? ` +${waits.length - 1}` : ""}` })
        : t.info && t.info.queued_in
          ? el("small", { textContent: `queued in ${t.info.queued_in.name}` })
          : null;
    return el(
      "li",
      {},
      el(
        "button",
        {
          type: "button",
          className: `fwb-row${waits.length || t.state === "blocked" ? " is-waiting" : ""}`,
          "data-fw-task": t.id,
          title: `${t.id}: ${t.state}`,
          onclick: () => activate(key, t),
        },
        el("span", { className: "fwb-row-id", textContent: t.id }),
        right,
      ),
    );
  }

  return { mount, paint, onFlywheel, laneOf, LANES };
};

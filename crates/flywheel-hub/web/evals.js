// flywheel hub: fw eval in the Flywheel panel. The folder's eval runs
// (each `fw eval runs --json` row: set, variants, progress, spend against
// the budget) as a drawer section; a run opens its board -- each variant
// against the baseline with the interval, the smallest difference the run
// could detect, and the verdict (better / worse / tie / no information);
// the overall numbers; and the task x variant grid, one mark per trial
// (pass, fail, suspect, contaminated, voided, infra, pending). A mark opens
// that trial: its outcome, cost, trace checks, per-check scores, reader
// review, final diff, and oracle log. Read-only: runs are started,
// reviewed, and decided with `fw eval` (the board says which command).
// flywheel.js renders the section; protocol: GET /api/flywheel/eval
// ?folder&run[&trial] (`fw eval report --json`, one trial's detail).
"use strict";

window.FwHubEvals = function (ctx) {
  const { el, icon, api, ago, section, via, pathOf, toast } = ctx;
  const { dollars } = window.FwHubFmt;

  const ui = { folder: null, run: null, report: null, trial: null, detail: null, loading: false };

  const MARK_LABEL = {
    pass: "passed (the held-out oracle)",
    fail: "failed the oracle",
    suspect: "passed, but a reader doubts it: fw eval decide",
    contaminated: "read outside its trial (counts as a fail)",
    voided: "a pass the operator voided (counts as a fail)",
    infra: "the machinery failed (left out)",
    pending: "not run yet",
  };
  const pct = (x) => (x === null || x === undefined ? "n/a" : `${Math.round(x * 100)}%`);
  const money = (x) => (x === null || x === undefined ? "n/a" : dollars(x));
  const signed = (x, unit) => `${x >= 0 ? "+" : ""}${x.toFixed(1)}${unit}`;

  // --- the board (a sheet, built once) ---------------------------------

  const board = el("dialog", { id: "dlg-eval", className: "sheet ev-sheet", "aria-label": "Eval run" });
  board.addEventListener("click", (event) => {
    if (event.target.closest("[data-close]") || event.target === board) board.close("cancel");
  });
  board.addEventListener("close", () => {
    ui.run = null;
    ui.report = null;
    ui.trial = null;
    ui.detail = null;
    if (!document.querySelector("dialog[open]") && ctx.focusTerminal) ctx.focusTerminal();
  });
  document.body.append(board);

  async function openRun(folder, runId) {
    ui.folder = folder;
    ui.run = runId;
    ui.report = null;
    ui.trial = null;
    ui.detail = null;
    ui.loading = true;
    renderBoard();
    if (!board.open) board.showModal();
    try {
      ui.report = await api("GET", `${via(folder)}/api/flywheel/eval?folder=${encodeURIComponent(pathOf(folder))}&run=${encodeURIComponent(runId)}`);
    } catch (error) {
      ui.report = { error: String(error.message || error) };
    }
    ui.loading = false;
    if (ui.run === runId) renderBoard();
  }

  async function openTrial(trialId) {
    const runId = ui.run;
    ui.trial = trialId;
    ui.detail = null;
    renderBoard();
    try {
      const detail = await api(
        "GET",
        `${via(ui.folder)}/api/flywheel/eval?folder=${encodeURIComponent(pathOf(ui.folder))}&run=${encodeURIComponent(runId)}&trial=${encodeURIComponent(trialId)}`,
      );
      if (ui.run === runId && ui.trial === trialId) ui.detail = detail;
    } catch (error) {
      if (ui.trial === trialId) ui.detail = { error: String(error.message || error) };
      toast(`trial ${trialId}: ${error.message || error}`);
    }
    if (ui.run === runId) {
      renderBoard();
      const open = board.querySelector(".ev-trial");
      if (open) open.scrollIntoView({ block: "nearest" });
    }
  }

  function verdictChip(c) {
    return el("span", { className: `ev-verdict ${c.verdict}`, textContent: c.verdict.replace("_", " ") });
  }

  function comparisonBlock(cmp) {
    const rows = cmp.comparisons.map((c) => {
      const unit = c.metric === "success" ? " pts" : "%";
      const primary = cmp.primary.includes(c.metric);
      const saturated = c.verdict === "no_information";
      // The interval on a shared scale: zero in the middle.
      const span = Math.max(Math.abs(c.ci_low), Math.abs(c.ci_high), Math.abs(c.mde), 1);
      const at = (x) => `${50 + (x / span) * 48}%`;
      return el(
        "li",
        { className: `ev-cmp${primary ? " primary" : ""}`, dataset: { evMetric: c.metric, evVerdict: c.verdict } },
        el("b", { textContent: `${c.metric}${primary ? " *" : ""}`, title: primary ? "primary (declared before the run)" : "exploratory" }),
        saturated
          ? el("span", { className: "ev-cmp-text", textContent: `no information: every task saturated (${c.tasks} tasks)` })
          : el("span", { className: "ev-cmp-text", textContent: `${signed(c.delta, unit)}  [${signed(c.ci_low, unit)}, ${signed(c.ci_high, unit)}]  MDE ${c.mde.toFixed(1)}${unit}` }),
        saturated
          ? el("span", { className: "ev-ci" })
          : el(
            "span",
            { className: "ev-ci", title: "95% interval; the band is the smallest detectable difference" },
            el("i", { className: "ev-ci-mde", style: `left:${at(-c.mde)};right:${100 - parseFloat(at(c.mde))}%` }),
            el("i", { className: "ev-ci-zero" }),
            el("i", { className: "ev-ci-bar", style: `left:${at(c.ci_low)};right:${100 - parseFloat(at(c.ci_high))}%` }),
            el("i", { className: "ev-ci-dot", style: `left:${at(c.delta)}` }),
          ),
        verdictChip(c),
      );
    });
    return el(
      "section",
      { className: "ev-block" },
      el("h3", { textContent: `${cmp.variant} vs ${cmp.baseline}` }),
      el("ul", { className: "ev-cmps" }, rows),
    );
  }

  function overallTable(r) {
    const head = ["variant", "tasks", "success", "pass@k", "pass^k", "cost/success", "turns/success", "abstain"];
    return el(
      "table",
      { className: "ev-table" },
      el("thead", {}, el("tr", {}, head.map((h) => el("th", { textContent: h })))),
      el(
        "tbody",
        {},
        r.overall.map((o) =>
          el(
            "tr",
            {},
            el("th", { textContent: o.variant }),
            el("td", { textContent: String(o.tasks) }),
            el("td", { textContent: pct(o.success) }),
            el("td", { textContent: pct(o.pass_at_k) }),
            el("td", { textContent: pct(o.pass_hat_k) }),
            el("td", { textContent: money(o.cost_per_success) }),
            el("td", { textContent: o.turns_per_success === null ? "n/a" : String(Math.round(o.turns_per_success)) }),
            el("td", { textContent: pct(o.abstain_accuracy) }),
          )),
      ),
    );
  }

  function grid(r) {
    const labels = r.variants.map((v) => v.label);
    return el(
      "table",
      { className: "ev-grid" },
      el("thead", {}, el("tr", {}, el("th", { textContent: "task" }), labels.map((l) => el("th", { textContent: l })))),
      el(
        "tbody",
        {},
        r.tasks.map((t) =>
          el(
            "tr",
            { dataset: { evTask: t.task } },
            el(
              "th",
              {},
              el(
                "span",
                { className: "ev-task-cell" },
                el("span", { className: "ev-task", textContent: t.task, title: t.task }),
                el("small", { className: `ev-tier ${t.tier}`, textContent: t.kind === "dont-act" ? "don't act" : t.tier }),
              ),
            ),
            labels.map((l) => {
              const cell = t.cells[l];
              return el(
                "td",
                {},
                el(
                  "span",
                  { className: "ev-marks" },
                  (t.marks[l] || []).map((m) =>
                    el("button", {
                      type: "button",
                      className: `ev-mark ${m.mark}${ui.trial === m.trial ? " open" : ""}`,
                      title: `${m.trial}: ${MARK_LABEL[m.mark] || m.mark} · ${dollars(m.cost_usd)}`,
                      "aria-label": `${m.trial}: ${m.mark}`,
                      dataset: { evTrial: m.trial, evMark: m.mark },
                      onclick: () => openTrial(m.trial),
                    })),
                ),
                cell && cell.n ? el("small", { textContent: `${cell.passes}/${cell.n} · ${money(cell.cost_per_success)}` }) : null,
              );
            }),
          )),
      ),
    );
  }

  function pre(text, className) {
    return text ? el("pre", { className: `ev-pre ${className || ""}`, textContent: text }) : el("p", { className: "fo-note", textContent: "(none kept)" });
  }

  function trialPanel() {
    if (!ui.trial) return null;
    const d = ui.detail;
    const kids = [el("h3", { textContent: ui.trial })];
    if (!d) kids.push(el("p", { className: "fo-note", textContent: "reading..." }));
    else if (d.error) kids.push(el("p", { className: "fo-error", textContent: d.error }));
    else {
      const row = d.row;
      const trace = row.trace || {};
      const facts = [
        ["oracle", row.oracle || "-"],
        ["validity", row.validity || "unchecked"],
        ["final commit", `${(row.final_commit || "").slice(0, 10)} (${row.final_where || "-"})`],
        ["flywheel", row.flywheel_status || "-"],
        ["cost", dollars(row.cost_usd)],
        ["turns", String(row.turns)],
        ["attempts", String(row.attempts)],
        ["agent time", `${Math.round(row.agent_secs / 60)} min`],
        ["check.sh runs", String(trace.check_sh_runs ?? "-")],
        ["skill calls", String(trace.skill_calls ?? "-")],
        ["tool calls", String(trace.tool_calls ?? "-")],
      ];
      kids.push(el("dl", { className: "ev-facts" }, facts.map(([k, v]) => el("div", {}, el("dt", { textContent: k }), el("dd", { textContent: v })))));
      if (row.validity_detail) kids.push(el("p", { className: "fo-note", textContent: row.validity_detail }));
      if (d.checks.length) {
        kids.push(el("ul", { className: "ev-checks" }, d.checks.map((c) =>
          el("li", { className: c.passed ? "ok" : "fail" }, el("b", { textContent: `${c.source}:${c.id}` }), el("span", { textContent: c.note || (c.passed ? "pass" : "fail") })))));
      }
      if (d.review) {
        kids.push(el(
          "div",
          { className: `ev-review ${d.review.verdict}` },
          el("b", { textContent: `reader (${d.review.model}): ${d.review.verdict}, ${d.review.confidence}${d.review.decision ? ` · operator ${d.review.decision}` : ""}` }),
          el("p", { textContent: d.review.notes }),
        ));
        if (row.validity === "suspect") {
          kids.push(el("p", { className: "fo-note" }, "Decide in a terminal: ", el("code", { textContent: `fw eval decide ${ui.run} ${ui.trial} --confirm|--clear` })));
        }
      }
      kids.push(el("details", { className: "ev-more", open: true }, el("summary", { textContent: "final diff" }), pre(d.diff, "diff")));
      kids.push(el("details", { className: "ev-more" }, el("summary", { textContent: "oracle log" }), pre(d.oracle_log)));
      kids.push(el("p", { className: "fo-note", textContent: `artifacts: ${d.dir}` }));
    }
    return el("section", { className: "ev-trial", dataset: { evTrialOpen: ui.trial } }, kids);
  }

  function renderBoard() {
    const r = ui.report;
    const head = el(
      "header",
      { className: "sheet-head fo-head" },
      el(
        "div",
        { className: "fo-title" },
        el("span", { className: "sheet-kicker", textContent: "eval run" }),
        el("h2", { textContent: ui.run || "" }),
        r && !r.error
          ? el("span", { className: "fo-sub", textContent: `${r.set} · k=${r.k} · ${r.status} · ${r.trials_graded}/${r.trials_total} graded${r.trials_infra ? `, ${r.trials_infra} infra` : ""}${r.trials_pending ? `, ${r.trials_pending} pending` : ""} · ${dollars(r.spent_usd)}${r.budget_usd ? ` of ${dollars(r.budget_usd)}` : ""}` })
          : null,
      ),
      el("div", { className: "fo-head-actions" }, el("button", { type: "button", className: "icon", "data-close": true, title: "Close (Esc)", "aria-label": "Close eval run" }, icon("x"))),
    );
    const body = el("div", { className: "fo-body ev-body" });
    if (ui.loading) body.append(el("p", { className: "fo-empty", textContent: "reading the run..." }));
    else if (!r) body.append(el("p", { className: "fo-empty", textContent: "no run" }));
    else if (r.error) body.append(el("p", { className: "fo-error", textContent: r.error }));
    else {
      r.comparisons.forEach((c) => body.append(comparisonBlock(c)));
      if (r.comparisons.some((c) => c.primary.length)) {
        body.append(el("p", { className: "fo-note", textContent: "* primary: declared before the run; the rest are exploratory. A tie means the difference is inside what this run can detect." }));
      }
      body.append(el("section", { className: "ev-block" }, el("h3", { textContent: "overall (macro over build tasks)" }), el("div", { className: "ev-scroll" }, overallTable(r))));
      if (r.saturated_tasks.length) {
        body.append(el("p", { className: "fo-note", textContent: `saturated (every trial passed in every variant; no success information): ${r.saturated_tasks.join(", ")}` }));
      }
      body.append(el("section", { className: "ev-block" }, el("h3", { textContent: "trials" }), el("div", { className: "ev-scroll" }, grid(r))));
      const fast = Object.entries(r.worth_a_read).filter(([, ids]) => ids.length);
      if (fast.length) {
        body.append(el("p", { className: "fo-note", textContent: `fastest passes, worth a read: ${fast.map(([v, ids]) => `${v}: ${ids.join(", ")}`).join(" · ")}` }));
      }
      const t = trialPanel();
      if (t) body.append(t);
    }
    const scroll = board.querySelector(".ev-body") ? board.querySelector(".ev-body").scrollTop : 0;
    board.replaceChildren(head, body);
    body.scrollTop = scroll;
  }

  // --- the drawer section ----------------------------------------------

  function runRow(folder, run) {
    const done = run.done || 0;
    const total = run.total || 0;
    const segs = [];
    if (total) {
      const passed = run.passed || 0;
      const failed = Math.max(done - passed - (run.infra || 0), 0);
      if (passed) segs.push(el("i", { className: "fw-seg verified", style: `flex:${passed}` }));
      if (failed) segs.push(el("i", { className: "fw-seg failed", style: `flex:${failed}` }));
      if (run.infra) segs.push(el("i", { className: "fw-seg blocked", style: `flex:${run.infra}` }));
      if (run.running) segs.push(el("i", { className: "fw-seg running", style: `flex:${run.running}` }));
      const pending = total - done - (run.running || 0);
      if (pending > 0) segs.push(el("i", { className: "fw-seg pending", style: `flex:${pending}` }));
    }
    const status = run.status === "paused" ? "awaiting_approval" : run.status === "done" ? "done" : "running";
    return el(
      "li",
      { className: "fw-run ev-run", dataset: { evRun: run.run_id } },
      el(
        "div",
        { className: "fw-run-line" },
        el("span", { className: `fw-status ${status}`, textContent: run.status }),
        el("b", { textContent: `${run.set} · ${run.variants.join(" vs ")}`, title: run.run_id }),
        el("span", { className: "fw-run-when", textContent: run.started_at ? `${ago(run.started_at)} ago` : "" }),
        el("button", {
          type: "button",
          className: "text-btn fw-out",
          textContent: "open",
          title: `Open eval run ${run.run_id}`,
          dataset: { evOpen: run.run_id },
          onclick: (event) => {
            event.stopPropagation();
            openRun(folder, run.run_id);
          },
        }),
      ),
      el("div", { className: "fw-bar ev-bar", title: `${run.passed || 0} passed of ${done} graded, ${total} trials` }, segs),
      el(
        "div",
        { className: "fw-run-sub" },
        el("small", { textContent: `${done}/${total} trials · k=${run.k}` }),
        el("small", { className: "fw-run-cost", textContent: `${dollars(run.spent_usd)}${run.budget_usd ? ` of ${dollars(run.budget_usd)}` : ""}` }),
      ),
      run.status === "paused"
        ? el("small", { className: "fw-run-error", textContent: `paused at its budget: fw eval replay --resume ${run.run_id} --budget USD` })
        : null,
    );
  }

  function evalSection(folder, panel) {
    const runs = panel.evals || [];
    if (!runs.length) return null;
    return section("Evals", runs.length, el("ul", { className: "fw-list" }, runs.map((r) => runRow(folder, r))));
  }

  return { section: evalSection, openRun };
};

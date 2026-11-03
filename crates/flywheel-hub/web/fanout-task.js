// flywheel hub: a verified fan-out and a flywheel task, both ways.
// Promote (from the fan-out board): write the fan-out as a task of its
// repo's directory work source -- the prompt as context, the graders as
// command graders, the winner's commit as a reference -- shown first word
// for word (path and bytes), written only on confirm (the dry run's
// fingerprint), as an uncommitted file; then "Run with worker here" so the
// flywheel loop verifies it again. Fan out this task (from the Flywheel
// panel): a launch form seeded with the task's goal, context, and command
// graders; its held-out graders are added by the hub at launch, run in
// every worktree, and never shown to the agents. app-base.js creates it;
// protocol: POST /api/fanouts/{id}/promote, GET /api/flywheel/task, and
// POST /api/fanouts {"from_task"}.
"use strict";

window.FwHubFanoutTask = function (ctx) {
  const { el, api, toast } = ctx;

  const dialog = el("dialog", { id: "dlg-fanout-task", className: "sheet fo-new fo-task", "aria-label": "Promote to flywheel task" });
  dialog.addEventListener("click", (event) => {
    if (event.target.closest("[data-close]") || event.target === dialog) dialog.close("cancel");
  });
  dialog.addEventListener("close", () => {
    clearTimeout(form && form.timer);
    form = null;
    if (!document.querySelector("dialog[open]")) ctx.focusTerminal();
  });
  document.body.append(dialog);

  let form = null;

  const field = (label, hint, ...children) =>
    el("label", { className: "field" }, el("span", { className: "field-label" }, label, hint ? el("span", { className: "field-hint", textContent: ` ${hint}` }) : null), ...children);

  // The reference choices: the default (the winner, if any), each other
  // passing candidate, none.
  function referenceOptions(f) {
    const options = [["", f.winner ? `the winner, ${f.winner.label} (default)` : "none: no winner yet (default)"]];
    for (const c of f.candidates) {
      if (c.verdict === "pass" && (!f.winner || c.label !== f.winner.label)) options.push([c.label, `${c.label}: passes (${c.kind}${c.model ? `:${c.model}` : ""})`]);
    }
    if (f.winner) options.push(["none", "none"]);
    return options;
  }

  function request() {
    const body = {
      task_id: form.taskId.value.trim() || undefined,
      phase: form.phase.value.trim() || undefined,
      goal: form.goal.value.trim() || undefined,
      reference: form.reference.value || undefined,
    };
    return body;
  }

  function showPlan(answer) {
    const plan = answer.plan;
    form.preview.replaceChildren(
      el("p", { className: "fo-plan-summary" },
        "Writes ", el("code", { textContent: plan.relative }), plan.new_phase ? ` (new phase ${plan.phase})` : ` (phase ${plan.phase})`),
      ...(plan.notes || []).map((n) => el("p", { className: "fo-plan-detail", textContent: n })),
      ...(answer.warnings || []).map((w) => el("p", { className: "fo-plan-warn", textContent: `warning: ${w}` })),
      el("pre", { className: "fo-cmds fo-task-file", textContent: plan.contents }),
    );
    form.phases.replaceChildren(...(plan.phases || []).map((p) => el("option", { value: p })));
  }

  async function preview() {
    if (!form) return;
    const seq = ++form.seq;
    const mine = form;
    mine.write.disabled = true;
    mine.fingerprint = null;
    mine.error.hidden = true;
    try {
      const answer = await api("POST", `/api/fanouts/${mine.f.id}/promote`, { ...request(), dry_run: true });
      if (form !== mine || seq !== mine.seq) return;
      if (!mine.filled) {
        // The defaults, as the server drafted them, become the fields.
        mine.filled = true;
        mine.taskId.value = answer.plan.task_id;
        mine.phase.value = answer.plan.phase;
        try {
          mine.goal.value = JSON.parse(answer.plan.contents).goal || "";
        } catch (_) {
          // The preview still shows the file.
        }
      }
      mine.fingerprint = answer.fingerprint;
      mine.write.disabled = false;
      showPlan(answer);
    } catch (error) {
      if (form !== mine || seq !== mine.seq) return;
      mine.error.hidden = false;
      mine.error.textContent = error.message;
      mine.preview.replaceChildren();
    }
  }

  function previewSoon() {
    if (!form) return;
    form.write.disabled = true;
    clearTimeout(form.timer);
    form.timer = setTimeout(preview, 350);
  }

  async function write() {
    const mine = form;
    if (!mine || !mine.fingerprint) return;
    mine.write.disabled = true;
    mine.write.textContent = "Writing…";
    let done;
    try {
      done = await api("POST", `/api/fanouts/${mine.f.id}/promote`, { ...request(), confirm: mine.fingerprint });
    } catch (error) {
      mine.write.textContent = "Write task file";
      mine.error.hidden = false;
      mine.error.textContent = error.message;
      // The plan may have moved under it: show what would be written now.
      if (form === mine) preview();
      return;
    }
    if (form !== mine) return;
    written(mine.f, done);
    ctx.onPromoted(done.fanout);
  }

  // After the write: what was written, and the way into the loop.
  function written(f, done) {
    const w = done.written;
    const folder = f.folder || f.repo;
    dialog.replaceChildren(el(
      "form",
      { method: "dialog" },
      el("header", { className: "sheet-head" }, el("span", { className: "sheet-kicker", textContent: "flywheel task" }), el("h2", { textContent: `Wrote ${w.task_id}` })),
      el("div", { className: "fo-act-body" },
        el("p", { className: "fo-plan-summary" }, el("code", { textContent: w.relative }), " is a new uncommitted file in ", el("code", { textContent: folder }), "."),
        el("p", {
          className: "fo-plan-detail",
          textContent: w.listed
            ? "Its directory work source lists it: `fw worker` there picks it up and verifies it through the loop (its graders, then the held-out gate and landing). Commit the file to keep it with the repo."
            : "Written, but the directory work source does not list it (see the Flywheel panel's problem line).",
        }),
        ...(done.warnings || []).map((x) => el("p", { className: "fo-plan-warn", textContent: `warning: ${x}` })),
        queueNote(w.queue || [])),
      el("footer", { className: "sheet-foot" },
        el("button", { type: "submit", className: "slab ghost", value: "", textContent: "Close" }),
        el("button", { type: "button", className: "slab ghost", textContent: "Open Flywheel panel", onclick: () => {
          dialog.close();
          ctx.openFlywheel(folder, f.workspace_id);
        } }),
        el("button", { type: "button", className: "slab", textContent: "Run with worker here", title: "Start fw worker in a new shell session in this folder: it drains every active task not done yet, this one included", onclick: () => {
          dialog.close();
          ctx.startWorker(folder, f.workspace_id);
        } })),
    ));
    dialog.querySelector(".sheet-foot .slab:not(.ghost)").focus();
  }

  // `fw worker` takes no task: it drains the whole active queue.
  function queueNote(queue) {
    if (!queue.length) return null;
    return el("p", { className: "fo-plan-warn", textContent: `fw worker drains every active task, not only this one: ${queue.length} other${queue.length === 1 ? "" : "s"} not done yet may run first (${queue.join(", ")}).` });
  }

  // A fan-out seeded from a task: that task is the durable one already.
  // Re-verify it with the worker, or write a separate task deliberately.
  function promote(f) {
    if (!f.origin) return promoteForm(f, "");
    const o = f.origin;
    const folder = f.folder || f.repo;
    dialog.replaceChildren(el(
      "form",
      { method: "dialog" },
      el("header", { className: "sheet-head" }, el("span", { className: "sheet-kicker", textContent: "flywheel task" }), el("h2", { textContent: `${f.name} came from task ${o.task_id}` })),
      el("div", { className: "fo-act-body" },
        el("p", { className: "fo-plan-summary" }, "Task ", el("code", { textContent: o.phase ? `${o.phase}/${o.task_id}` : o.task_id }), " is already the durable spec and graders for this work", o.held_out ? `, held-out oracle${o.held_out === 1 ? "" : "s"} included` : "", ". To verify it through the loop, run the worker in this folder."),
        el("p", { className: "fo-plan-detail", textContent: "fw worker drains every active task not done yet, in phase order, this one included. A separate task (a new id) leaves the original's held-out oracles behind." })),
      el("footer", { className: "sheet-foot" },
        el("button", { type: "submit", className: "slab ghost", value: "", textContent: "Close" }),
        el("button", { type: "button", className: "slab ghost", textContent: "Write a separate task…", onclick: () => promoteForm(f, `${o.task_id}-${f.id.slice(0, 4)}`) }),
        el("button", { type: "button", className: "slab", textContent: `Run ${o.task_id} with worker here`, onclick: () => {
          dialog.close();
          ctx.startWorker(folder, f.workspace_id);
        } })),
    ));
    if (!dialog.open) dialog.showModal();
    dialog.querySelector(".sheet-foot .slab:not(.ghost)").focus();
  }

  function promoteForm(f, suggestedId) {
    const taskId = el("input", { type: "text", maxLength: 100, spellcheck: false, autocomplete: "off", placeholder: "default: the fan-out's name and tag" });
    const phases = el("datalist", { id: "fo-task-phases" });
    const phase = el("input", { type: "text", maxLength: 100, spellcheck: false, autocomplete: "off", placeholder: "default: a new phase after the others" });
    phase.setAttribute("list", "fo-task-phases");
    const goal = el("textarea", { rows: 2, maxLength: 600, placeholder: "One or two sentences: the outcome, not the procedure" });
    const reference = el("select", { "aria-label": "Reference commit" }, referenceOptions(f).map(([v, label]) => el("option", { value: v, textContent: label })));
    const previewBox = el("div", { className: "fo-plan fo-task-preview", "aria-live": "polite" }, el("p", { className: "fo-plan-detail", textContent: "Drafting…" }));
    const error = el("p", { className: "fo-error", role: "alert", hidden: true });
    const writeButton = el("button", { type: "button", className: "slab", textContent: "Write task file", disabled: true });
    const node = el(
      "form",
      { method: "dialog", novalidate: true },
      el("header", { className: "sheet-head" }, el("span", { className: "sheet-kicker", textContent: "verified fan-out" }), el("h2", { textContent: `Promote ${f.name} to a flywheel task` })),
      el("p", { className: "sheet-body", textContent: "A durable task in this repo's directory work source: the prompt as context, the graders as command graders, the chosen commit as a reference (prior work, not the solution). The flywheel worker then verifies it through the loop. Nothing is written until you confirm what is shown below; the hub never commits." }),
      el("div", { className: "fo-two" }, field("Task id", "", taskId), field("Phase", "active/<phase>/", phase, phases)),
      field("Goal", "", goal),
      field("Reference commit", "", reference),
      error,
      previewBox,
      el("footer", { className: "sheet-foot" }, el("button", { type: "button", className: "slab ghost", "data-close": true, textContent: "Cancel" }), writeButton),
    );
    form = { f, taskId, phase, phases, goal, reference, preview: previewBox, error, write: writeButton, seq: 0, timer: null, fingerprint: null, filled: false };
    for (const input of [taskId, phase, goal]) input.addEventListener("input", previewSoon);
    reference.addEventListener("change", previewSoon);
    writeButton.addEventListener("click", write);
    node.addEventListener("submit", (event) => event.preventDefault());
    taskId.value = suggestedId;
    dialog.replaceChildren(node);
    if (!dialog.open) dialog.showModal();
    taskId.focus();
    preview();
  }

  // "Fan out this task": the launch form, seeded.
  async function fanOutTask(folder, workspaceId, taskId) {
    let seed;
    try {
      seed = await api("GET", `/api/flywheel/task?folder=${encodeURIComponent(folder)}&task=${encodeURIComponent(taskId)}`);
    } catch (error) {
      toast(`Cannot fan out ${taskId}: ${error.message}`, { tone: "bad" });
      return;
    }
    ctx.openLaunch(workspaceId, seed);
  }

  // The board's word on where a fan-out stands with flywheel.
  function boardNotes(f) {
    const notes = [];
    if (f.origin) {
      notes.push(el("span", { className: "fo-note fo-origin", textContent: `from flywheel task ${f.origin.task_id}${f.origin.held_out ? ` · ${f.origin.held_out} held-out grader${f.origin.held_out === 1 ? "" : "s"} from its registry` : ""}` }));
    }
    const folder = f.folder || f.repo;
    for (const p of f.promoted || []) {
      const where = p.state === "archived" ? " · archived (done and landed)" : p.state === "gone" ? " · file no longer there" : "";
      notes.push(el("button", {
        type: "button",
        className: "text-btn fo-note fo-origin",
        title: `${p.current || `${p.path} (no longer there)`}\nOpen the Flywheel panel`,
        textContent: `promoted to flywheel task ${p.task_id} (${p.phase})${where}`,
        onclick: () => ctx.openFlywheel(folder, f.workspace_id),
      }));
    }
    return notes;
  }

  return { promote, fanOutTask, boardNotes };
};

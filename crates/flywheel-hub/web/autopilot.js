// flywheel hub: autopilot in the Flywheel panel. A folder's intake daemon
// (`fw autopilot`): its state with start / stop / pause / resume, what its
// last cycle decided and why (emitted tasks, dropped findings with their
// reasons, tier verdicts), the tasks it emitted with their recorded score
// and latest run, and the manual gates its work waits on with approve /
// deny -- each confirmed, each the CLI's own code path on the server
// (`fw autopilot` in a hub session, its pause marker, `fw approve` /
// `fw reject` pinned to the gate shown). The held-out gate, risk tiers
// and team-mode reviews are shown, never decided here. Another machine's
// folders are read-only. flywheel.js renders what this returns; protocol:
// POST /api/flywheel/autopilot/{start,stop,pause,resume,decide}.
"use strict";

window.FwHubAutopilot = function (ctx) {
  const { el, icon, state, toast, confirmDialog, openSession, post } = ctx;

  // Deny forms open, by run id (their feedback survives re-renders).
  const denying = new Map();
  let busy = false;

  const tierLabel = (t) => `T${t}`;

  // The hub sessions running this folder's daemon (this hub's only).
  function sessions(key) {
    const folder = ctx.pathOf(key);
    return state.sessions.filter((s) =>
      !s.machine && s.cwd === folder && s.state === "running" && !s.fallback_command &&
      Array.isArray(s.command) && s.command[3] === "fw-autopilot" && s.worker && s.worker.running);
  }

  // The gates its work lands through, in words.
  function gates(config) {
    const out = [`submit ${config.submit_strategy || "merge"}`];
    if (config.submit_verify) out.push("submit verify");
    if (config.held_out) out.push("held-out gate");
    if (config.risk_tiers && config.risk_tiers.length) out.push(`risk tiers ${config.risk_tiers.join("/")}`);
    if (config.team_mode) out.push("team mode: GitHub reviews");
    return out;
  }

  async function act(key, verb, body) {
    if (busy) return null;
    busy = true;
    try {
      const out = await post(`/api/flywheel/autopilot/${verb}`, { folder: ctx.pathOf(key), ...body });
      if (!out.ok) {
        toast((out.data && out.data.error) || `autopilot ${verb} failed: HTTP ${out.status}`, { tone: "bad", timeout: 8000 });
        if (out.status === 409 && out.data && out.data.running) openSession(out.data.running.id);
        ctx.reload();
        return null;
      }
      ctx.reload();
      return out.data;
    } catch (error) {
      toast(`autopilot ${verb} failed: ${error.message}`, { tone: "bad" });
      return null;
    } finally {
      busy = false;
    }
  }

  async function start(key, panel) {
    const pilot = panel.intake;
    const config = pilot.config;
    const ok = await confirmDialog({
      kicker: "autopilot",
      title: `Start autopilot in ${ctx.labelFor(key)}?`,
      body: `Runs fw autopilot, the neverending intake daemon, in a new shell session here. Every ${config.interval_seconds}s it discovers work, scores it, and writes tasks until ${config.target_depth} are queued; a worker lands them on the base branch (${config.landing}) unattended, through: ${gates(config).join(", ")}.${pilot.paused ? " Its pause marker is set: it starts held until you resume it." : ""}`,
      ok: "Start autopilot",
    });
    if (!ok) return;
    const workspace = ctx.workspaceFor(key);
    const view = await act(key, "start", { confirm: true, workspace_id: workspace ? workspace.id : undefined, ...ctx.termSize() });
    if (!view) return;
    await ctx.refresh();
    ctx.close();
    openSession(view.id, { split: "down" });
    toast(el("span", {}, "Started ", el("b", { textContent: view.name }), ` in ${ctx.labelFor(key)}`), { timeout: 3500 });
  }

  async function stop(key) {
    const out = await act(key, "stop", {});
    if (out) toast(`Stopping autopilot in ${ctx.labelFor(key)}: it finishes gracefully.`, { timeout: 3500 });
  }

  async function decide(key, gate, approve) {
    const feedback = approve ? "" : (denying.get(gate.run_id) || "").trim();
    const ok = await confirmDialog({
      kicker: "autopilot gate",
      title: `${approve ? "Approve" : "Deny"} ${gate.task_id}?`,
      body: approve
        ? `Approve gate ${gate.gate} (attempt ${gate.attempt}) of run ${gate.run_id}: "${gate.instruction}". Bound to this gate only; the next worker run on ${gate.task_id} applies it (it advances to the next gate, or lands).`
        : `Deny gate ${gate.gate} (attempt ${gate.attempt}) of run ${gate.run_id}. The run goes to failed validation${feedback ? " and your feedback goes into its next attempt" : " (no feedback given)"}.`,
      ok: approve ? "Approve" : "Deny",
    });
    if (!ok) return;
    const body = { run: gate.run_id, decision: approve ? "approve" : "deny", confirm: true, gate: gate.gate, attempt: gate.attempt };
    if (feedback) body.feedback = feedback;
    const done = await act(key, "decide", body);
    if (!done) return;
    denying.delete(gate.run_id);
    toast(`${approve ? "Approved" : "Denied"} ${gate.task_id} (command #${done.command_id}): the worker applies it on its next run.`, { timeout: 5000 });
  }

  // --- the daemon row ----------------------------------------------------

  function detail(pilot, fallback) {
    if (!pilot) return fallback.detail;
    const parts = [pilot.state];
    if (pilot.pid) parts.push(`pid ${pilot.pid}`);
    if (pilot.cycle) parts.push(`cycle ${pilot.cycle}`);
    if (pilot.live && pilot.next_cycle_at && pilot.state === "idle") {
      parts.push(`next in ${ctx.span(Math.max(0, (Date.parse(pilot.next_cycle_at) - Date.now()) / 1000))}`);
    }
    if (pilot.paused && pilot.state !== "paused") parts.push("pause marker set");
    return parts.join(" · ");
  }

  function actions(key, panel) {
    const pilot = panel.intake;
    if (!pilot || ctx.machineOfKey(key)) return null;
    const ours = sessions(key);
    const button = (text, title, onclick, cls = "text-btn") =>
      el("button", { type: "button", className: cls, textContent: text, title, onclick, disabled: busy });
    const out = [];
    if (ours.length) {
      out.push(button("session", "Open the autopilot's terminal", () => { ctx.close(); openSession(ours[0].id); }));
      out.push(button("stop", "Stop it: Ctrl-C to its session (it stops gracefully; the session stays a shell)", () => stop(key)));
    }
    if (pilot.live || pilot.paused) {
      out.push(pilot.paused
        ? button("resume", "Let it run cycles again (fw autopilot --resume)", () => act(key, "resume", {}))
        : button("pause", "Hold it between cycles (fw autopilot --pause): a running cycle finishes first", () => act(key, "pause", {})));
    }
    if (!pilot.live) {
      const broken = !!panel.problem && !panel.source;
      out.push(el("button", {
        type: "button",
        className: "slab fw-start",
        disabled: broken || busy,
        title: broken ? "The policy does not load: fix flywheel.toml first" : "Run fw autopilot in a new shell session here (asks first: its work lands unattended)",
        onclick: () => start(key, panel),
      }, icon("plus"), "Start autopilot"));
    }
    return el("span", { className: "fw-daemon-actions" }, out);
  }

  function row(key, panel, daemonRow) {
    const pilot = panel.intake;
    const daemon = { live: panel.autopilot.live, detail: detail(pilot, panel.autopilot) };
    return daemonRow("autopilot", daemon, actions(key, panel));
  }

  // --- the section ---------------------------------------------------------

  function chip(text, cls, title) {
    return el("span", { className: `ap-chip${cls ? ` ${cls}` : ""}`, textContent: text, title });
  }

  function gateRow(key, gate) {
    const readOnly = !!ctx.machineOfKey(key);
    const open = denying.has(gate.run_id);
    const form = open
      ? el(
        "div",
        { className: "ap-deny" },
        el("textarea", {
          rows: 2,
          maxLength: 4000,
          placeholder: "Feedback for the next attempt (optional)",
          "aria-label": `Feedback denying ${gate.task_id}`,
          value: denying.get(gate.run_id),
          oninput: (event) => denying.set(gate.run_id, event.target.value),
        }),
        el(
          "span",
          { className: "ap-deny-actions" },
          el("button", { type: "button", className: "text-btn", textContent: "cancel", onclick: () => { denying.delete(gate.run_id); ctx.render(); } }),
          el("button", { type: "button", className: "slab danger", textContent: "Deny", onclick: () => decide(key, gate, false) }),
        ),
      )
      : null;
    return el(
      "li",
      { className: `ap-gate${gate.autopilot ? " ours" : ""}` },
      el(
        "div",
        { className: "ap-gate-line" },
        el("span", { className: "fw-light awaiting", textContent: "gate" }),
        el(
          "span",
          { className: "fw-task-text" },
          el("b", { textContent: gate.task_id, title: gate.run_id }),
          el("small", { textContent: gate.instruction || "manual approval", title: gate.instruction }),
        ),
        el(
          "span",
          { className: "fw-task-meta" },
          el("small", { textContent: `gate ${gate.gate} · try ${gate.attempt}${gate.since ? ` · ${ctx.ago(gate.since)}` : ""}` }),
          ctx.outputButton(gate.run_id, gate.task_id),
        ),
      ),
      readOnly
        ? null
        : el(
          "div",
          { className: "ap-gate-actions" },
          open ? null : el("button", { type: "button", className: "text-btn", textContent: "deny...", onclick: () => { denying.set(gate.run_id, ""); ctx.render(); } }),
          el("button", { type: "button", className: "slab ap-approve", textContent: "Approve", onclick: () => decide(key, gate, true) }),
        ),
      form,
    );
  }

  function candidateRow(c) {
    const breakdown = c.preemptive
      ? `preemptive: interrupt base + urgency ${c.urgency}`
      : `tier weight ${c.tier_weight} · urgency ${c.urgency} · importance ${c.importance} · unblocks ${c.blocks} · effort ${c.effort}`;
    const light = c.state || "archived";
    return el(
      "li",
      { className: "ap-cand" },
      el("span", { className: `fw-light ${c.state || "pending"}`, textContent: light }),
      el(
        "span",
        { className: "fw-task-text" },
        el("b", { textContent: c.task_id, title: c.goal }),
        el("small", { textContent: `${breakdown} · grader ${c.grader_source}${c.assumptions.length ? ` · assumes: ${c.assumptions.join("; ")}` : ""}`, title: `${c.goal}\n${c.authoritative_grader}` }),
      ),
      el(
        "span",
        { className: "fw-task-meta" },
        chip(tierLabel(c.tier), c.preemptive ? "hot" : "", `tier ${c.tier}${c.preemptive ? " (preemptive)" : ""}`),
        el("small", { className: "ap-score", textContent: Number(c.score).toFixed(1), title: "recorded final score" }),
        ctx.outputButton(c.run_id, c.task_id),
      ),
    );
  }

  function decisionBox(decision) {
    if (!decision) return el("p", { className: "fw-empty", textContent: "No cycle recorded yet." });
    return el(
      "div",
      { className: "ap-decision" },
      el(
        "p",
        { className: "ap-why" },
        el("b", { textContent: `Cycle ${decision.cycle}: ` }),
        `emitted ${decision.emitted}, dropped ${decision.dropped}. `,
        el("span", { textContent: decision.reason }),
      ),
      decision.dropped_findings.length
        ? el(
          "ul",
          { className: "ap-drops" },
          decision.dropped_findings.map((d) => el(
            "li",
            {},
            chip(tierLabel(d.tier)),
            el("b", { textContent: d.title || d.finding_id, title: d.finding_id }),
            el("small", { textContent: d.reason }),
          )),
        )
        : null,
      el("p", { className: "ap-meta", textContent: `discovery cache ${decision.discovery_cache || "-"}` }),
    );
  }

  function tierStrip(tiers) {
    return el(
      "div",
      { className: "ap-tiers", role: "list", "aria-label": "Tier verdicts of the last cycle" },
      tiers.map((t) => el(
        "span",
        {
          role: "listitem",
          className: `ap-tier ${t.verdict.replace(" ", "-")}${t.preemptive ? " pre" : ""}`,
          title: `Tier ${t.tier}: ${t.name} (${t.preemptive ? "preemptive" : `scheduled, weight ${t.weight}`}): ${t.verdict}${t.emitted ? `, ${t.emitted} emitted` : ""}`,
        },
        String(t.tier),
        t.emitted ? el("i", { textContent: String(t.emitted) }) : null,
      )),
    );
  }

  function section(key, panel) {
    const pilot = panel.intake;
    if (!pilot) return null;
    const ours = pilot.gates.filter((g) => g.autopilot);
    const others = pilot.gates.filter((g) => !g.autopilot);
    const config = pilot.config;
    const body = [
      el("p", { className: "ap-config", textContent: `target ${config.target_depth} · every ${config.interval_seconds}s · lands via ${config.landing} · ${gates(config).join(" · ")}` }),
      pilot.problem ? el("p", { className: "fw-problem", textContent: pilot.problem }) : null,
      tierStrip(pilot.tiers),
      decisionBox(pilot.decision),
    ];
    if (pilot.gates.length) {
      body.push(el("h4", { className: "ap-sub", textContent: `Waiting on you (${pilot.gates.length})` }));
      body.push(el("ul", { className: "fw-list ap-gates" }, [...ours, ...others].map((g) => gateRow(key, g))));
    }
    if (pilot.holds.length) {
      body.push(el("h4", { className: "ap-sub", textContent: "Held by a gate" }));
      body.push(el("ul", { className: "fw-list" }, pilot.holds.map((h) => el(
        "li",
        { className: "fw-alert stopped" },
        el("span", { className: "fw-kind stopped", textContent: h.kind }),
        el("span", { className: "fw-task-text" }, el("b", { textContent: h.task_id }), el("small", { textContent: h.detail, title: h.detail })),
        ctx.outputButton(h.run_id, h.task_id),
      ))));
    }
    if (pilot.candidates.length) {
      body.push(el("h4", { className: "ap-sub", textContent: "Emitted, newest first" }));
      body.push(el("ul", { className: "fw-list ap-cands" }, pilot.candidates.map(candidateRow)));
    }
    return ctx.section("Autopilot", ours.length ? `${ours.length} need${ours.length === 1 ? "s" : ""} you` : pilot.state, ...body);
  }

  // Approve / deny on a Needs-you "awaiting" alert, when its gate is read.
  function alertActions(key, panel, alert) {
    if (alert.kind !== "awaiting" || !alert.run_id || ctx.machineOfKey(key) || !panel.intake) return null;
    const gate = panel.intake.gates.find((g) => g.run_id === alert.run_id);
    if (!gate) return null;
    return el(
      "span",
      { className: "ap-alert-actions" },
      el("button", { type: "button", className: "text-btn", textContent: "approve", onclick: () => decide(key, gate, true) }),
      el("button", { type: "button", className: "text-btn", textContent: "deny", onclick: () => decide(key, gate, false) }),
    );
  }

  return { row, section, alertActions };
};

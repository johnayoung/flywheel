// flywheel hub: the attention list. Every agent that needs you, across all
// workspaces, heading the sidebar's Everything list (and the phone's
// start screen):
// blocked ones first (longest waiting first), then agents whose turn
// ended asking you something (telemetry's `asking`: a reply, a decision,
// or a command only you can run; answered in the row: the reply goes
// through POST /api/broadcast, typed in now or queued), then prompt queues that
// stalled (queue.js: the agent exited, a paste was refused), then fan-outs whose graders
// finished since this browser looked (fanout.js; their candidates' own
// finished turns are not listed), then finished turns this browser has
// not looked at (newest first), then flywheel tasks that failed,
// stranded, stopped, or wait on a gate (flywheel.js; a folder with many
// is one row, and alerts marked seen in its panel drop out). Tap one to open it (a flywheel row opens its folder's
// Flywheel panel, a fan-out row its board).
"use strict";

window.FwHubAttention = function (ctx) {
  const { $, el, icon, age, state, workspaceById, agentStatus, openSession, flywheel, fanout } = ctx;

  // What an asking session's telemetry says it asks (else null).
  function askOf(session) {
    const snap = ctx.telemetryOf ? ctx.telemetryOf(session.id) : null;
    return snap && snap.status === "asking" && snap.ask ? snap.ask : null;
  }
  const ASK_WORD = { answer: "asks", decide: "decide", run: "run this", handoff: "hand off" };
  // Handoff prompts being edited, by session.
  const handoffs = new Map();
  // Replies being typed, by session: a re-render keeps them.
  const drafts = new Map();

  // Agents that need this browser, most urgent first. A fan-out
  // candidate's finished turn is not one: the hub grades it, and its
  // fan-out's row says when the verdicts are in.
  function entries() {
    const out = [];
    const graded = fanout ? fanout.candidateSessions() : new Set();
    for (const session of state.sessions) {
      const agent = agentStatus(session);
      if (!agent || (agent.status !== "blocked" && agent.status !== "done")) continue;
      if (agent.status === "done" && graded.has(session.id)) continue;
      const ask = agent.status === "done" ? askOf(session) : null;
      out.push({ session, agent, ask, level: agent.status === "blocked" ? 0 : ask ? 1 : 2 });
    }
    const time = (e) => (e.ask ? e.ask.since : Date.parse(e.agent.since)) || 0;
    // Blocked and asking: longest waiting first; finished: newest first.
    out.sort((a, b) => a.level - b.level || (a.level < 2 ? time(a) - time(b) : time(b) - time(a)));
    return out;
  }

  // A worktree session's branch (the badge cache's, else the one it was
  // cut on): which of an agent's parallel tasks this is.
  const branchOf = (session) =>
    session.worktree ? (session.git && session.git.branch) || session.worktree.branch : null;

  async function reply(session, text, form) {
    const prompt = String(text || "").trim();
    if (!prompt) return;
    if (form) form.classList.add("busy");
    try {
      const done = await ctx.api("POST", "/api/broadcast", { targets: [session.id], text: prompt });
      const result = (done.results || [])[0] || {};
      if (result.result === "refused") {
        ctx.toast(`Not sent to ${session.name}: ${result.why || "refused"}`, { tone: "bad" });
        return;
      }
      drafts.delete(session.id);
      ctx.toast(result.result === "queued" ? `Queued for ${session.name}` : `Sent to ${session.name}`);
    } catch (error) {
      ctx.toast(`Not sent to ${session.name}: ${error.message}`, { tone: "bad" });
    } finally {
      if (form) form.classList.remove("busy");
    }
  }

  // Accept a handoff: a new session in the same directory continues the
  // work from the (possibly edited) prompt; it opens.
  async function startSuccessor(session, prompt, button) {
    if (button) button.disabled = true;
    try {
      const next = await ctx.api("POST", `/api/sessions/${encodeURIComponent(session.id)}/continue`, { prompt });
      handoffs.delete(session.id);
      ctx.toast(`Started ${next.name}, continuing ${session.name}`);
      // The new session is listed before it is opened.
      if (ctx.refresh) await ctx.refresh();
      openSession(next.id);
    } catch (error) {
      ctx.toast(`Not started: ${error.message}`, { tone: "bad" });
    } finally {
      if (button) button.disabled = false;
    }
  }

  // A handoff: the new session's prompt, editable, and the button that
  // starts it.
  function handoffBox(session, ask) {
    const area = el("textarea", {
      className: "attn-handoff-text",
      rows: 5,
      spellcheck: false,
      "aria-label": `${session.name}'s prompt for the new session`,
      value: handoffs.has(session.id) ? handoffs.get(session.id) : ask.prompt || "",
      oninput: (event) => handoffs.set(session.id, event.target.value),
    });
    const start = el("button", {
      type: "button",
      className: "slab attn-start",
      textContent: "Start session",
      title: "Start a new session here with this prompt; it continues this one's work",
      onclick: () => startSuccessor(session, area.value, start),
    });
    return el("div", { className: "attn-handoff attn-reply" }, area, start);
  }

  // An ask answered in place: its command to copy, its options to pick,
  // and a one-line reply.
  function askBox(session, ask) {
    const field = el("input", {
      type: "text",
      className: "attn-reply-input",
      placeholder: ask.kind === "run" ? "Paste the output, or reply" : "Reply",
      "aria-label": `Reply to ${session.name}`,
      value: drafts.get(session.id) || "",
      oninput: (event) => drafts.set(session.id, event.target.value),
    });
    const form = el(
      "form",
      {
        className: "attn-reply",
        onsubmit: (event) => {
          event.preventDefault();
          reply(session, field.value, form);
        },
      },
      field,
      el("button", { type: "submit", className: "slab attn-send", textContent: "Send" }),
    );
    return el(
      "div",
      { className: "attn-ask" },
      ask.command
        ? el(
            "div",
            { className: "attn-cmd" },
            el("pre", { textContent: ask.command }),
            el("button", {
              type: "button",
              className: "slab ghost attn-copy",
              textContent: "Copy",
              title: "Copy the command; run it, then paste its output as the reply",
              onclick: async () => {
                const ok = await ctx.copyText(ask.command);
                ctx.toast(ok ? "Command copied" : "Could not copy", { tone: ok ? "" : "bad" });
              },
            }),
          )
        : null,
      ask.kind === "handoff" && ask.prompt ? handoffBox(session, ask) : null,
      (ask.options || []).length
        ? el(
            "div",
            { className: "attn-options" },
            ask.options.map((option) =>
              el("button", { type: "button", className: "chip attn-option", textContent: option, title: `Reply "${option}"`, onclick: () => reply(session, option) }),
            ),
          )
        : null,
      form,
    );
  }

  function row({ session, agent, ask }) {
    const workspace = workspaceById(session.workspace_id);
    const active = state.attached === session.id;
    const branch = branchOf(session);
    return el(
      "li",
      {},
      el(
        "button",
        {
          type: "button",
          className: `attn-row ${ask ? "asking" : agent.status}${active ? " active" : ""}`,
          dataset: { id: session.id },
          "aria-current": active ? "true" : undefined,
          onclick: () => openSession(session.id),
        },
        el("span", { className: `glyph ${session.kind}` }, icon(session.kind === "codex" ? "codex" : "claude")),
        el(
          "span",
          { className: "attn-text" },
          el(
            "span",
            { className: "attn-line" },
            el("b", { className: "attn-name", textContent: session.name }),
            session.machine
              ? el("span", { className: "machine-badge", textContent: session.machine, title: `on machine ${session.machine}` })
              : null,
            el("span", { className: "attn-ws", textContent: workspace ? workspace.name : "" }),
            branch
              ? el("span", { className: "attn-branch", title: `worktree branch ${branch}` }, icon("branch"), el("span", { textContent: branch }))
              : null,
          ),
          el("span", {
            className: "attn-detail",
            textContent: agent.status === "blocked"
              ? (agent.detail || "waiting on you")
              : ask ? ask.text : "finished its turn",
            title: ask ? ask.text : "",
          }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: `attn-status ${ask ? "asking" : agent.status}`, textContent: ask ? ASK_WORD[ask.kind] || "asks" : agent.status }),
          el("span", { className: "attn-age", textContent: age(ask ? new Date(ask.since).toISOString() : agent.since) }),
        ),
      ),
      ask ? askBox(session, ask) : null,
    );
  }

  // A flywheel task that needs you: opens its folder's panel.
  function flywheelRow({ folder, label, alert, count }) {
    return el(
      "li",
      {},
      el(
        "button",
        {
          type: "button",
          className: `attn-row fw ${alert.kind}`,
          onclick: () => flywheel.open(folder),
          title: count > 1
            ? `${label}: ${alert.detail}. Open its Flywheel panel.`
            : `${alert.subject} in ${label}: ${alert.detail}`,
        },
        el("span", { className: "glyph fw" }, icon("wheel")),
        el(
          "span",
          { className: "attn-text" },
          el(
            "span",
            { className: "attn-line" },
            el("b", { className: "attn-name", textContent: alert.subject }),
            el("span", { className: "attn-ws", textContent: label }),
          ),
          el("span", { className: "attn-detail", textContent: alert.detail }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: `attn-status fw ${alert.kind}`, textContent: alert.kind }),
        ),
      ),
    );
  }

  // The sidebar's list, and the phone's start screen (no session open:
  // the drawer is closed, so the list is the screen; app.css shows it on
  // narrow screens only).
  // A reply being typed is never torn down under the user: the render
  // waits for the field to lose focus.
  let held = false;
  const typing = () => !!(document.activeElement && document.activeElement.closest(".attn-reply"));
  document.addEventListener("focusout", (event) => {
    if (!held || !event.target.closest || !event.target.closest(".attn-reply")) return;
    // Focus moving to the row's own Send button is not leaving it.
    setTimeout(() => {
      if (held && !typing()) {
        held = false;
        render();
      }
    }, 0);
  });

  function render() {
    if (typing()) {
      held = true;
      return;
    }
    held = false;
    const list = entries();
    const loop = flywheel ? flywheel.attention() : [];
    const graded = fanout ? fanout.attention() : [];
    const queues = ctx.queue ? ctx.queue().attention() : [];
    for (const root of [$("#attention"), $("#empty-attention")]) {
      if (!root) continue;
      const any = list.length || loop.length || graded.length || queues.length;
      root.hidden = !any;
      root.replaceChildren(...(any ? content(list, loop, graded, queues) : []));
    }
  }

  function content(list, loop, graded, queues) {
    const blocked = list.filter((e) => e.level === 0).length;
    const asking = list.filter((e) => e.level === 1).length;
    const alerts = loop.reduce((n, e) => n + (e.count || 1), 0);
    return [
      el(
        "header",
        { className: "attn-head" },
        el("span", { className: "attn-title", textContent: "Needs you" }),
        el("span", {
          className: "attn-count",
          textContent: [
            blocked ? `${blocked} blocked` : "",
            asking ? `${asking} asking` : "",
            list.length - blocked - asking ? `${list.length - blocked - asking} done` : "",
            alerts ? `${alerts} flywheel` : "",
            graded.length ? `${graded.length} fan-out${graded.length === 1 ? "" : "s"}` : "",
            queues.length ? `${queues.length} queue${queues.length === 1 ? "" : "s"} stalled` : "",
          ]
            .filter(Boolean)
            .join(" / "),
        }),
      ),
      // Blocked agents, then fan-out verdicts, then finished turns.
      el(
        "ul",
        { className: "attn-list" },
        list.filter((e) => e.level < 2).map(row),
        queues.map((s) => ctx.queue().attentionRow(s)),
        graded.map((f) => fanout.attentionRow(f)),
        list.filter((e) => e.level === 2).map(row),
        loop.map(flywheelRow),
      ),
    ];
  }

  // Ages tick while the list shows.
  setInterval(() => {
    if (document.visibilityState === "visible" && !$("#attention").hidden) render();
  }, 30000);

  return { render, entries, branchOf };
};

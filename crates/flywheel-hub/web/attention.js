// flywheel hub: the attention list. Every agent that needs you, across all
// workspaces, at the top of the sidebar (the phone's session switcher):
// blocked ones first (longest waiting first), then prompt queues that
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
      out.push({ session, agent });
    }
    const time = (e) => Date.parse(e.agent.since) || 0;
    out.sort((a, b) => {
      if (a.agent.status !== b.agent.status) return a.agent.status === "blocked" ? -1 : 1;
      return a.agent.status === "blocked" ? time(a) - time(b) : time(b) - time(a);
    });
    return out;
  }

  // A worktree session's branch (the badge cache's, else the one it was
  // cut on): which of an agent's parallel tasks this is.
  const branchOf = (session) =>
    session.worktree ? (session.git && session.git.branch) || session.worktree.branch : null;

  function row({ session, agent }) {
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
          className: `attn-row ${agent.status}${active ? " active" : ""}`,
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
              : "finished its turn",
          }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: `attn-status ${agent.status}`, textContent: agent.status }),
          el("span", { className: "attn-age", textContent: age(agent.since) }),
        ),
      ),
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
  function render() {
    const list = entries();
    const loop = flywheel ? flywheel.attention() : [];
    const graded = fanout ? fanout.attention() : [];
    const queues = ctx.queue ? ctx.queue().attention() : [];
    for (const root of [$("#attention"), $("#empty-attention")]) {
      if (!root) continue;
      const any = list.length || loop.length || graded.length || queues.length;
      // A re-render keeps where its own short scroll was (a row the
      // phone's "N in Needs you" line scrolled to stays in view).
      const kept = root.querySelector(".attn-list")?.scrollTop || 0;
      root.hidden = !any;
      root.replaceChildren(...(any ? content(list, loop, graded, queues) : []));
      const inner = root.querySelector(".attn-list");
      if (inner) {
        inner.scrollTop = kept;
        inner.addEventListener("scroll", () => moreBelow(inner), { passive: true });
        moreBelow(inner);
      }
    }
  }

  // Rows past the bottom of a capped list fade out there: there is more.
  function moreBelow(list) {
    list.classList.toggle("more-below", list.scrollTop + list.clientHeight < list.scrollHeight - 2);
  }

  // Bring these sessions' rows into view (the list scrolled to the first,
  // then the box onto the screen) and mark them a moment.
  function reveal(ids) {
    const box = $("#attention");
    if (!box || box.hidden) return;
    const list = box.querySelector(".attn-list");
    const rows = [...box.querySelectorAll(".attn-row[data-id]")].filter((r) => ids.includes(r.dataset.id));
    if (list && rows.length) {
      list.scrollTop += rows[0].getBoundingClientRect().top - list.getBoundingClientRect().top;
      moreBelow(list);
    }
    box.scrollIntoView({ block: "start", behavior: "smooth" });
    for (const r of rows) {
      r.classList.remove("pointed");
      void r.offsetWidth;
      r.classList.add("pointed");
    }
  }

  function content(list, loop, graded, queues) {
    const blocked = list.filter((e) => e.agent.status === "blocked").length;
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
            list.length - blocked ? `${list.length - blocked} done` : "",
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
        list.filter((e) => e.agent.status === "blocked").map(row),
        queues.map((s) => ctx.queue().attentionRow(s)),
        graded.map((f) => fanout.attentionRow(f)),
        list.filter((e) => e.agent.status !== "blocked").map(row),
        loop.map(flywheelRow),
      ),
    ];
  }

  // Ages tick while the list shows.
  setInterval(() => {
    if (document.visibilityState === "visible" && !$("#attention").hidden) render();
  }, 30000);

  return { render, entries, branchOf, reveal };
};

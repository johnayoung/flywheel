// flywheel hub: the attention list. Every agent that needs you, across all
// workspaces, at the top of the sidebar (the phone's session switcher):
// blocked ones first (longest waiting first), then finished turns this
// browser has not looked at (newest first). Tap one to open it.
"use strict";

window.FwHubAttention = function (ctx) {
  const { $, el, icon, state, workspaceById, agentStatus, openSession } = ctx;

  function age(since) {
    const seconds = Math.max(0, (Date.now() - Date.parse(since)) / 1000);
    if (!Number.isFinite(seconds)) return "";
    if (seconds < 60) return "now";
    if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
    if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
    return `${Math.floor(seconds / 86400)}d`;
  }

  // Agents that need this browser, most urgent first.
  function entries() {
    const out = [];
    for (const session of state.sessions) {
      const agent = agentStatus(session);
      if (!agent || (agent.status !== "blocked" && agent.status !== "done")) continue;
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

  // The sidebar's list, and the phone's start screen (no session open:
  // the drawer is closed, so the list is the screen; app.css shows it on
  // narrow screens only).
  function render() {
    const list = entries();
    for (const root of [$("#attention"), $("#empty-attention")]) {
      if (!root) continue;
      root.hidden = list.length === 0;
      root.replaceChildren(...(list.length ? content(list) : []));
    }
  }

  function content(list) {
    const blocked = list.filter((e) => e.agent.status === "blocked").length;
    return [
      el(
        "header",
        { className: "attn-head" },
        el("span", { className: "attn-title", textContent: "Needs you" }),
        el("span", {
          className: "attn-count",
          textContent: [blocked ? `${blocked} blocked` : "", list.length - blocked ? `${list.length - blocked} done` : ""]
            .filter(Boolean)
            .join(" / "),
        }),
      ),
      el("ul", { className: "attn-list" }, list.map(row)),
    ];
  }

  // Ages tick while the list shows.
  setInterval(() => {
    if (document.visibilityState === "visible" && !$("#attention").hidden) render();
  }, 30000);

  return { render, entries, branchOf };
};

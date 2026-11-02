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

  function row({ session, agent }) {
    const workspace = workspaceById(session.workspace_id);
    const active = state.attached === session.id;
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

  function render() {
    const root = $("#attention");
    const list = entries();
    root.hidden = list.length === 0;
    if (!list.length) {
      root.replaceChildren();
      return;
    }
    const blocked = list.filter((e) => e.agent.status === "blocked").length;
    root.replaceChildren(
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
    );
  }

  // Ages tick while the list shows.
  setInterval(() => {
    if (document.visibilityState === "visible" && !$("#attention").hidden) render();
  }, 30000);

  return { render, entries };
};

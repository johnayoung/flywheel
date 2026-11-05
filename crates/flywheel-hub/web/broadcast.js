// flywheel hub: broadcast -- one prompt to several agent sessions at once.
// Each target gets it through its prompt queue's own path on the hub that
// owns it (POST /api/broadcast): typed in now when the agent is at rest
// and nobody types into it, else queued until its turn is done (never
// while it is blocked), or refused with why. A selection spanning
// machines is one call per machine, through this hub's proxy
// (/m/<machine>/api/broadcast); the answers are merged into one result
// list.
//
// Ways in: Ctrl/Cmd-click agent rows in the sidebar (the selection bar
// under the workspaces sends them), a workspace's "Send to all agents",
// the sidebar's Broadcast button, and the palette. The dialog (a
// full-width sheet on a phone) lists every agent session to pick from,
// grouped by workspace, with its status. app-base.js creates it.
"use strict";

window.FwHubBroadcast = function (ctx) {
  const { $, el, icon, rawApi, machines, state, sessionById, workspaceById, isAgent, statusLook, glyphFor } = ctx;
  const toast = (...args) => ctx.toast(...args);

  const HEADER_KEY = "fw-hub.broadcast-header";
  const RESULT_LABEL = { sent: "sent", queued: "queued", refused: "refused" };

  const dialog = $("#dlg-broadcast");
  const list = $("#bc-targets");
  const text = $("#bc-text");
  const header = $("#bc-header");
  const note = $("#bc-note");
  const results = $("#bc-results");
  const sendButton = $("#bc-send");
  const count = $("#bc-count");
  const bar = $("#bc-bar");

  const ui = {
    picked: new Set(), // the sidebar's selection (session ids)
    chosen: new Set(), // the dialog's
    busy: false,
    sent: false, // the dialog shows a result list
  };

  // Agent sessions a prompt can be broadcast to (not exited), in sidebar
  // order.
  const candidates = () => state.sessions.filter((s) => isAgent(s) && s.state !== "exited");
  const machineOf = (s) => (s && s.machine) || "";

  function setNote(message, tone) {
    note.textContent = message || "";
    note.dataset.tone = tone || "";
  }

  // --- the sidebar's selection ----------------------------------------------

  // A Ctrl/Cmd-click on a row: toggle it (agents only). `true`: handled.
  function toggle(session) {
    if (!isAgent(session)) return false;
    if (ui.picked.has(session.id)) ui.picked.delete(session.id);
    else ui.picked.add(session.id);
    ctx.render();
    return true;
  }

  const isPicked = (id) => ui.picked.has(id);

  function clearPicked() {
    ui.picked.clear();
    ctx.render();
  }

  // The selection bar and the sidebar button follow the session list.
  function render() {
    for (const id of [...ui.picked]) {
      if (!sessionById(id)) ui.picked.delete(id);
    }
    const n = ui.picked.size;
    bar.hidden = n === 0;
    if (n) {
      bar.replaceChildren(
        el("span", { className: "bc-bar-count", textContent: `${n} agent${n === 1 ? "" : "s"} selected` }),
        el("button", { type: "button", className: "slab", onclick: () => open([...ui.picked]) }, icon("broadcast"), "Send prompt"),
        el("button", { type: "button", className: "slab ghost", textContent: "Clear", onclick: clearPicked }),
      );
    }
    $("#bc-open").hidden = candidates().length === 0;
  }

  // A workspace's tool: its agents, when it has any.
  function workspaceTool(workspace, iconButton) {
    const agents = candidates().filter((s) => s.workspace_id === workspace.id);
    if (!agents.length) return null;
    return iconButton("broadcast", `Send to all agents in ${workspace.name}`, () => openWorkspace(workspace.id));
  }

  // --- the dialog -------------------------------------------------------------

  function targetRow(session) {
    const st = statusLook(session);
    const workspace = workspaceById(session.workspace_id);
    const offline = !!session.machine && !machines.reachable(session.machine);
    const box = el("input", {
      type: "checkbox",
      checked: ui.chosen.has(session.id),
      disabled: offline,
      onchange: (e) => {
        if (e.target.checked) ui.chosen.add(session.id);
        else ui.chosen.delete(session.id);
        renderCount();
      },
    });
    return el(
      "label",
      { className: `bc-target${offline ? " off" : ""}`, title: offline ? `machine ${session.machine} is not reachable` : "" },
      box,
      el("span", { className: `glyph ${glyphFor(session.kind)}` }, icon(glyphFor(session.kind))),
      el("span", { className: "bc-name", textContent: session.name }),
      el("span", { className: "bc-where", textContent: [workspace ? workspace.name : "", session.machine ? `@${session.machine}` : ""].filter(Boolean).join(" ") }),
      el("span", { className: `bc-status ${st.tone || ""}`, dataset: { dot: st.dot }, textContent: st.label }),
    );
  }

  function renderTargets() {
    const pool = candidates();
    const groups = new Map();
    for (const s of pool) {
      if (!groups.has(s.workspace_id)) groups.set(s.workspace_id, []);
      groups.get(s.workspace_id).push(s);
    }
    list.replaceChildren(
      ...[...groups].map(([wsId, sessions]) => {
        const workspace = workspaceById(wsId);
        const all = sessions.every((s) => ui.chosen.has(s.id));
        return el(
          "div",
          { className: "bc-group" },
          el(
            "div",
            { className: "bc-group-head" },
            el("span", { textContent: workspace ? workspace.name : "workspace" }),
            workspace && workspace.machine ? el("span", { className: "machine-badge", textContent: workspace.machine }) : null,
            el("button", {
              type: "button",
              className: "text-btn",
              textContent: all ? "none" : "all",
              onclick: () => {
                for (const s of sessions) {
                  if (all) ui.chosen.delete(s.id);
                  else if (!s.machine || machines.reachable(s.machine)) ui.chosen.add(s.id);
                }
                renderTargets();
              },
            }),
          ),
          sessions.map(targetRow),
        );
      }),
    );
    if (!pool.length) list.append(el("p", { className: "bc-empty", textContent: "No Claude Code or Codex session to send to." }));
    renderCount();
  }

  function renderCount() {
    const n = [...ui.chosen].filter((id) => sessionById(id)).length;
    count.textContent = n ? `${n} selected` : "";
    sendButton.disabled = ui.busy || (!ui.sent && n === 0);
  }

  function showForm(shown) {
    list.closest(".field").hidden = !shown;
    text.closest("label").hidden = !shown;
    header.closest("label").hidden = !shown;
    results.hidden = shown;
    sendButton.textContent = shown ? "Send" : "Send another";
  }

  // Open the dialog with `ids` chosen.
  function open(ids) {
    ui.chosen = new Set((ids || []).filter((id) => isAgent(sessionById(id))));
    ui.sent = false;
    ui.busy = false;
    setNote("");
    try {
      header.checked = localStorage.getItem(HEADER_KEY) === "1";
    } catch (_) {
      header.checked = false;
    }
    showForm(true);
    renderTargets();
    document.body.classList.remove("side-open");
    if (!dialog.open) dialog.showModal();
    text.focus();
  }

  function openWorkspace(workspaceId) {
    open(candidates().filter((s) => s.workspace_id === workspaceId && (!s.machine || machines.reachable(s.machine))).map((s) => s.id));
  }

  // --- sending ----------------------------------------------------------------

  // One call per machine; a machine that fails refuses its targets.
  async function send() {
    if (ui.sent) {
      showForm(true);
      ui.sent = false;
      renderTargets();
      text.select();
      return;
    }
    const prompt = text.value;
    if (!prompt.trim()) {
      setNote("Write the prompt first.", "bad");
      text.focus();
      return;
    }
    const chosen = [...ui.chosen].map(sessionById).filter(Boolean);
    if (!chosen.length) {
      setNote("Pick at least one agent.", "bad");
      return;
    }
    const byMachine = new Map();
    for (const s of chosen) {
      const m = machineOf(s);
      if (!byMachine.has(m)) byMachine.set(m, []);
      byMachine.get(m).push(s);
    }
    const withHeader = header.checked;
    try {
      localStorage.setItem(HEADER_KEY, withHeader ? "1" : "0");
    } catch (_) {
      // Private mode: the choice is not kept.
    }
    ui.busy = true;
    renderCount();
    setNote(`Sending to ${chosen.length} agent${chosen.length === 1 ? "" : "s"}…`);
    const answers = await Promise.all(
      [...byMachine].map(async ([machine, sessions]) => {
        try {
          const done = await rawApi("POST", `${machines.prefix(machine)}/api/broadcast`, {
            targets: sessions.map((s) => s.id),
            text: prompt,
            header: withHeader,
          });
          return (done.results || []).map((r) => ({ ...r, machine }));
        } catch (error) {
          return sessions.map((s) => ({
            session_id: s.id,
            session: s.name,
            kind: s.kind,
            result: "refused",
            why: machine ? `machine ${machine}: ${error.message}` : error.message,
            machine,
          }));
        }
      }),
    );
    ui.busy = false;
    ui.sent = true;
    const merged = answers.flat();
    const tally = { sent: 0, queued: 0, refused: 0 };
    for (const r of merged) tally[r.result] = (tally[r.result] || 0) + 1;
    renderResults(merged);
    showForm(false);
    renderCount();
    const summary = `${tally.sent} sent, ${tally.queued} queued, ${tally.refused} refused`;
    setNote(summary, tally.refused ? "warn" : "");
    toast(`Broadcast: ${summary}.`, { tone: tally.refused && !tally.sent && !tally.queued ? "bad" : undefined });
    ui.picked.clear();
    ctx.render();
    ctx.refresh();
  }

  function renderResults(rows) {
    results.replaceChildren(
      ...rows.map((r) => {
        const session = r.session_id ? sessionById(r.session_id) : null;
        const name = r.session || r.target || "?";
        return el(
          "li",
          { className: `bc-result ${r.result}` },
          el("span", { className: "bc-tag", textContent: RESULT_LABEL[r.result] || r.result }),
          el(
            "span",
            { className: "bc-result-main" },
            session
              ? el("button", {
                type: "button",
                className: "text-btn bc-name",
                textContent: name,
                title: r.result === "queued" ? "Open its prompt queue" : "Open the session",
                onclick: () => {
                  dialog.close();
                  if (r.result === "queued") ctx.openQueue(session.id);
                  else ctx.openSession(session.id);
                },
              })
              : el("span", { className: "bc-name", textContent: name }),
            r.machine ? el("span", { className: "machine-badge", textContent: r.machine }) : null,
            r.why ? el("span", { className: "bc-why", textContent: r.why }) : null,
          ),
        );
      }),
    );
  }

  sendButton.addEventListener("click", send);
  text.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      send();
    }
  });

  function paletteItems(cmd) {
    if (!candidates().length) return;
    cmd("Broadcast a prompt", "one prompt to several agents: sent, or queued until each is done", () => open([...ui.picked]), "broadcast");
    for (const workspace of state.workspaces) {
      if (candidates().some((s) => s.workspace_id === workspace.id)) {
        cmd(`Send to all agents in ${workspace.name}`, workspace.machine ? `on ${workspace.machine}` : "broadcast", () => openWorkspace(workspace.id), "broadcast");
      }
    }
  }

  return { open, openWorkspace, toggle, isPicked, render, workspaceTool, paletteItems };
};

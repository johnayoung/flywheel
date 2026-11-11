// flywheel hub web client: session actions (create, relaunch, stop,
// close, rename) and the sidebar of workspaces and sessions, with the
// header it keeps in step.
"use strict";

// --- session actions -------------------------------------------------

function rememberWorkspace(id) {
  state.currentWorkspace = id;
  store.set("workspace", id);
}

// `place`: where it shows (see openSession); the focused pane by default.
async function createSession(body, place) {
  const created = await api("POST", "/api/sessions", { ...body, ...sizeHint() });
  await refresh();
  await openSession(created.id, place || {});
  return created;
}

// Relaunch in place: same record (name, place in the sidebar), a fresh
// process and scrollback; every pane showing it re-attaches.
async function relaunch(id) {
  if (!sessionById(id)) return;
  const showing = livePanes().filter((p) => p.sessionId === id);
  try {
    await api("POST", `/api/sessions/${id}/relaunch`, sizeHint());
    await refresh();
    if (!showing.length) {
      await openSession(id);
      return;
    }
    for (const pane of showing) {
      pane.show(id);
      pane.shownSeq = state.refreshSeq;
    }
    render();
  } catch (error) {
    if (error.status === 409) {
      // Relaunched already (another browser, the CLI): attach that one.
      await refresh().catch(() => {});
      const session = sessionById(id);
      if (session && session.state === "running" && session.live) {
        for (const pane of showing) {
          pane.show(id);
          pane.shownSeq = state.refreshSeq;
        }
        render();
        return;
      }
    }
    const pane = showing[0];
    if (pane) pane.showBanner(`Relaunch failed: ${error.message}`, pane.endedActions(id), false);
    else showBanner(`Relaunch failed: ${error.message}`, [], false);
  }
}

async function stopSession(id) {
  try {
    await api("POST", `/api/sessions/${id}/kill`);
    await refresh();
  } catch (error) {
    flagError(id, error);
  }
}

async function closeSession(id) {
  const session = sessionById(id);
  if (!session) return;
  let query = "";
  // A companion shell shares its agent's worktree: closing it never
  // removes that (the agent's close does).
  // Its reviewers go with it too (the server closes them along).
  const companions = state.sessions.filter((s) => s.companion_of === id || s.review_of === id);
  if (session.worktree && !session.companion_of && !session.review_of) {
    const how = await worktrees.confirmClose(session);
    if (!how) return;
    if (how.remove) query = `?remove_worktree=true${how.force ? "&force=true" : ""}`;
  } else if ((session.state === "running" && session.live) || companions.length) {
    const ok = await confirmDialog({
      kicker: "close session",
      title: `Close ${session.name}?`,
      body: [
        session.live && session.state === "running" ? "Its process" : "",
        session.live && session.state === "running" && session.live.pid ? ` (pid ${session.live.pid})` : "",
        session.live && session.state === "running"
          ? " is hung up and the scrollback is discarded. Every attached browser loses it."
          : "",
        companions.length ? ` ${companions.map((c) => c.name).join(", ")} ${companions.length === 1 ? "closes" : "close"} with it.` : "",
      ].join("").trim(),
      ok: "Kill and close",
    });
    if (!ok) return;
  }
  state.closing.add(id);
  try {
    // Closing an agent also closes its companion shells.
    const closing = companions.map((s) => s.id);
    closing.forEach((c) => state.closing.add(c));
    try {
      await api("DELETE", `/api/sessions/${id}${query}`);
    } finally {
      closing.forEach((c) => setTimeout(() => state.closing.delete(c), 3000));
    }
    // The hub closed the panes that showed them; refresh follows.
    await refresh();
  } catch (error) {
    flagError(id, error);
  } finally {
    state.closing.delete(id);
  }
}

// `refocus`: the user finished with Enter/Esc (not by clicking away), so
// the keyboard goes back to the terminal.
async function renameSession(id, name, refocus) {
  state.renaming = null;
  const session = sessionById(id);
  try {
    if (session && name.trim() && name.trim() !== session.name) {
      await api("PATCH", `/api/sessions/${id}`, { name });
      await refresh();
    } else {
      render();
    }
  } catch (error) {
    flagError(id, error);
  }
  if (refocus) focusTerminal();
}

function focusTerminal() {
  const term = focusedTerm();
  if (term && !document.querySelector("dialog[open]") && !paletteOpen() && !search.isOpen() && !$("#ctx-menu")) term.focus();
}

// Something over the terminal that owns the keys: a dialog, the
// palette, a menu, the Commands drawer, the Flywheel panel, Compose.
function overlayOpen() {
  return !!document.querySelector("dialog[open]") || paletteOpen() || search.isOpen() || !!$("#ctx-menu") ||
    commands.isOpen() || flywheel.isOpen() || review.isOpen() || !$("#compose").hidden;
}

// Whether focus may move to the terminal by itself (an attach, a stage
// rebuild): not out of an overlay or a field the user is in -- only from
// the stage, or from nowhere (the page itself, as after a closed pane).
function mayTakeFocus() {
  if (overlayOpen()) return false;
  const active = document.activeElement;
  if ($("#terminal").contains(active) || $("#pane-park").contains(active)) return true;
  // From nowhere only with a hardware keyboard: on a touch screen focus
  // would raise the on-screen keyboard unasked.
  return (!active || active === document.body) && !window.matchMedia("(pointer: coarse)").matches;
}

function startRename(id) {
  const session = sessionById(id);
  if (!session) return;
  state.collapsed.delete(session.workspace_id);
  if (window.matchMedia(NARROW).matches) document.body.classList.add("side-open");
  state.renaming = id;
  render();
  const input = $(`.session[data-id="${id}"] .rename`);
  if (input) {
    input.focus();
    input.select();
  }
}

function flagError(sessionId, error) {
  const session = sessionById(sessionId);
  if (session) state.wsErrors[session.workspace_id] = error.message;
  render();
}

async function deleteWorkspace(workspace) {
  const sessions = state.sessions.filter((s) => s.workspace_id === workspace.id);
  const running = sessions.filter((s) => s.state === "running");
  const { plural } = window.FwHubFmt;
  const ok = await confirmDialog({
    kicker: "delete workspace",
    title: `Delete ${workspace.name}?`,
    body: [
      running.length
        ? `${plural(running.length, "session")} still ${running.length === 1 ? "runs" : "run"} here: deleting kills ${running.length === 1 ? "it" : "them"}, and every attached browser loses ${running.length === 1 ? "it" : "them"}.`
        : "",
      sessions.length
        ? `Its ${plural(sessions.length, "session record")} ${sessions.length === 1 ? "goes" : "go"} with it.`
        : "",
      "The folders on disk are not touched.",
    ].filter(Boolean).join(" "),
    ok: running.length ? `Kill ${running.length} and delete` : "Delete",
  });
  if (!ok) return;
  // This browser removes them: no "removed elsewhere" notice.
  sessions.forEach((s) => state.closing.add(s.id));
  try {
    for (const session of running) {
      await api("DELETE", `/api/sessions/${session.id}`);
    }
    await api("DELETE", `/api/workspaces/${workspace.id}`);
    if (state.view === workspace.id) detach();
    delete state.layouts[workspace.id];
    delete state.confirmed[workspace.id];
    delete state.wsErrors[workspace.id];
    await refresh();
  } catch (error) {
    state.wsErrors[workspace.id] = error.message;
    render();
  } finally {
    sessions.forEach((s) => state.closing.delete(s.id));
  }
}

// --- sidebar ---------------------------------------------------------

function renderSession(session, index) {
  const st = statusLook(session);
  const active = state.attached === session.id;
  // In another pane on screen.
  const shown = !active && livePanes().some((p) => p.sessionId === session.id && onStage(p));
  const nameNode = state.renaming === session.id
    ? el("input", {
      className: "rename",
      value: session.name,
      maxLength: 100,
      "aria-label": "Session name",
      onclick: (e) => e.stopPropagation(),
      onkeydown: (e) => {
        e.stopPropagation();
        if (e.key === "Enter") renameSession(session.id, e.target.value, true);
        if (e.key === "Escape") {
          state.renaming = null;
          render();
          focusTerminal();
        }
      },
      onblur: (e) => {
        if (state.renaming === session.id) renameSession(session.id, e.target.value);
      },
    })
    : el("span", { className: "session-name", textContent: session.name, title: session.cwd });
  const actions = el(
    "span",
    { className: "session-actions" },
    iconButton("pen", "Rename (F2)", () => startRename(session.id)),
    session.state === "running" && session.live
      ? iconButton("stop", "Stop: hang up, keep the scrollback", () => stopSession(session.id))
      : iconButton("redo", "Relaunch in place", () => relaunch(session.id)),
    iconButton("x", "Close session", () => closeSession(session.id), true),
  );
  return el(
    "li",
    {
      className: `session${active ? " active" : ""}${shown ? " shown" : ""}${session.live ? "" : " gone"}${st.dot === "blocked" ? " needs-you" : ""}${session.companion_of || session.review_of ? " companion" : ""}${broadcast.isPicked(session.id) ? " picked" : ""}`,
      dataset: { id: session.id },
      tabIndex: 0,
      role: "button",
      "aria-current": active ? "true" : undefined,
      title: session.live ? undefined : goneText(session),
      // Drag onto a pane: its edge splits that way, its middle shows it there.
      draggable: !window.matchMedia(NARROW).matches,
      ondragstart: (e) => {
        dragged = session.id;
        e.dataTransfer.setData(DRAG_TYPE, session.id);
        e.dataTransfer.setData("text/plain", session.name);
        e.dataTransfer.effectAllowed = "copyMove";
      },
      // Ctrl/Cmd-click picks agents for a broadcast (broadcast.js).
      onclick: (e) => {
        if ((e.ctrlKey || e.metaKey) && !e.target.closest("button") && broadcast.toggle(session)) return;
        openSession(session.id);
      },
      ondblclick: (e) => {
        if (!e.target.closest("button")) startRename(session.id);
      },
      onkeydown: (e) => {
        if (e.target !== e.currentTarget) return;
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          e.currentTarget.click();
        } else if (e.key === "F2") {
          e.preventDefault();
          startRename(session.id);
        } else if (e.key === "Delete") {
          e.preventDefault();
          closeSession(session.id);
        }
      },
    },
    index < 9 ? el("span", { className: "num", textContent: index + 1 }) : null,
    el(
      "span",
      // Restored as a plain shell: the agent's glyph, dimmed, says so.
      {
        className: `glyph ${glyphFor(session.kind)}${session.fallback_command ? " as-shell" : ""}`,
        title: window.FwHubFmt.kindLabel(session),
      },
      icon(glyphFor(session.kind)),
    ),
    el("span", { className: `dot ${st.dot}`, title: st.title || st.label }),
    nameNode,
    el(
      "span",
      { className: "session-meta" },
      // No machine badge: a machine's rows sit under its own head.
      recBadge(session),
      el("span", { className: `session-state ${st.tone}`, textContent: st.label, title: st.title || undefined }),
      launcher.rowChip(session),
      promptQueue.rowChip(session),
      pullRequests.rowChip(session),
      actions,
    ),
    // What a blocked agent asks, readable without hover (touch).
    st.dot === "blocked" && st.detail
      ? el("span", { className: "session-detail", textContent: st.detail })
      : null,
    progressLine(session, st),
    // A companion (or a reviewer) sits under its agent: the agent's
    // line says it all.
    (session.companion_of && sessionById(session.companion_of)) || (session.review_of && sessionById(session.review_of))
      ? null
      : whereLine(session),
    // Author <-> reviewer (handoff.js).
    handoff.pairChip(session),
  );
}

// How far along a working (or blocked) agent is, from its telemetry: the
// plan it declared (a bar, done/total, the step it is on), else what is
// observed (the tool it is in); and its context window once that is
// nearly full. Nothing is shown for a session at rest.
function progressLine(session, st) {
  const t = sessionTelemetry.get(session.id);
  if (!t || (st.dot !== "working" && st.dot !== "blocked")) return null;
  const parts = [];
  if (t.plan && t.plan.total) {
    const bar = el("span", { className: "fwb-bar" }, el("i"));
    bar.firstChild.style.width = `${Math.round((t.plan.done / t.plan.total) * 100)}%`;
    parts.push(bar, el("b", { textContent: `${t.plan.done}/${t.plan.total}` }));
    if (t.plan.current) parts.push(el("span", { className: "session-step", textContent: t.plan.current }));
  } else if (t.tool) {
    parts.push(el("span", {
      className: "session-step tool",
      textContent: t.tool.summary ? `${t.tool.name}: ${t.tool.summary}` : t.tool.name,
    }));
  } else if (t.turn_title) {
    parts.push(el("span", { className: "session-step", textContent: t.turn_title }));
  }
  if (t.context && t.context.pct >= 80) {
    parts.push(el("span", {
      className: "session-ctx",
      textContent: `ctx ${t.context.exact === false ? "~" : ""}${Math.round(t.context.pct)}%`,
      title: "context window used",
    }));
  }
  if (!parts.length) return null;
  const said = t.plan && t.plan.current ? `step ${t.plan.done + 1} of ${t.plan.total}: ${t.plan.current}` : (t.tool && t.tool.summary) || t.turn_title || "";
  return el("span", { className: "session-progress", title: said }, parts);
}

// Where a session runs, as its workspace names it: the folder it was
// opened in (and the subdirectory, if deeper), for a worktree the folder
// it was cut from. A cwd outside every folder shows its last component.
function sessionFolder(session) {
  const trim = (path) => String(path || "").replace(/\/+$/, "") || "/";
  const cwd = trim(session.cwd);
  const folders = workspaceById(session.workspace_id)?.folders || [];
  const repo = session.worktree ? trim(session.worktree.repo) : null;
  const root = session.worktree ? trim(session.worktree.path) : null;
  let folder = null;
  let rest = "";
  if (repo) {
    folder = folders.find((f) => trim(f.path) === repo) || { path: repo };
    rest = cwd.startsWith(`${root}/`) ? cwd.slice(root.length + 1) : "";
  } else {
    for (const f of folders) {
      const path = trim(f.path);
      if ((cwd === path || cwd.startsWith(`${path}/`)) && (!folder || path.length > trim(folder.path).length)) {
        folder = f;
      }
    }
    if (folder) rest = cwd.slice(trim(folder.path).length + 1);
  }
  const label = folder ? folderLabel(folder) : basename(cwd);
  return { label: rest ? `${label}/${rest}` : label, outside: !folder };
}

// The line under a session row: its Claude account (when not the CLI's
// login), its folder, then its branch (worktree.js).
function whereLine(session) {
  if (!session.cwd) return worktrees.badge(session);
  const folder = sessionFolder(session);
  return el(
    "span",
    { className: "session-where" },
    session.account
      ? el("span", { className: "session-account", textContent: `@${session.account}`, title: `Claude account ${session.account} (fw hub account)` })
      : null,
    el("span", {
      className: `session-folder${folder.outside ? " outside" : ""}`,
      textContent: folder.label,
      title: folder.outside ? `${session.cwd} (outside the workspace's folders)` : session.cwd,
    }),
    worktrees.badge(session),
  );
}

// A session being recorded says so on its row.
function recBadge(session) {
  if (!session.recording) return null;
  return el("span", { className: "rec-dot row-rec", title: `Recording to ${session.recording}`, "aria-label": "recording" });
}

const machineBadge = (name) => el("span", { className: "machine-badge", textContent: name, title: `on machine ${name}` });

// The workspace's most urgent agent status (idle and unknown stay quiet),
// counted by the hub's one rule for done (docs/hub/internals.md): an open
// fan-out's candidates' finished turns are the hub's to grade, and a
// fan-out whose latest grading nobody has looked at is one done. The hub's
// rollup, Needs you, and `fw hub attach` count the same way.
function rollupChip(workspace) {
  let rolled = state.rollup[workspace.id];
  const graded = fanout.candidateSessions();
  const here = state.sessions.filter((s) => s.workspace_id === workspace.id);
  const counted = (status) =>
    here.filter((s) => agentStatus(s)?.status === status && !(status === "done" && graded.has(s.id))).length;
  let count;
  if (rolled === "blocked" || rolled === "working") {
    count = counted(rolled);
  } else {
    count = counted("done") + fanout.attention().filter((f) => f.workspace_id === workspace.id).length;
    rolled = count ? "done" : null;
  }
  if (!["blocked", "working", "done"].includes(rolled)) return null;
  return el("span", {
    className: `ws-status ${rolled}`,
    textContent: count > 1 ? `${count} ${rolled}` : rolled,
    title: `agents here: ${rolled}`,
  });
}

// On a phone the Needs you list (attention.js) sits right above the
// workspaces in the one scroll: a session it lists is not listed again
// under its workspace, which says how many it holds there instead.
function listedInNeedsYou() {
  return phone() ? new Set(attention.entries().map((e) => e.session.id)) : new Set();
}

function needsYouLine(ids) {
  return el(
    "li",
    {},
    el("button", {
      type: "button",
      className: "ws-needs",
      textContent: `${ids.length} in Needs you`,
      title: "Listed in Needs you, at the top",
      onclick: () => attention.reveal(ids),
    }),
  );
}

function renderWorkspace(workspace, i, numbering, needsYou = new Set()) {
  const sessions = state.sessions.filter((s) => s.workspace_id === workspace.id);
  const listed = sessions.filter((s) => !needsYou.has(s.id));
  const elsewhere = sessions.filter((s) => needsYou.has(s.id)).map((s) => s.id);
  const live = sessions.filter((s) => s.live && s.state === "running" && reachable(s)).length;
  const collapsed = state.collapsed.has(workspace.id);
  const attachedHere = sessionById(state.attached)?.workspace_id === workspace.id;
  const toggle = () => {
    if (state.collapsed.has(workspace.id)) state.collapsed.delete(workspace.id);
    else state.collapsed.add(workspace.id);
    store.set("collapsed", [...state.collapsed]);
    render();
  };
  const error = state.wsErrors[workspace.id];
  return el(
    "section",
    {
      className: `workspace${collapsed ? " collapsed" : ""}${attachedHere ? " current" : ""}`,
      style: `--i:${i}`,
      dataset: { id: workspace.id },
    },
    el(
      "div",
      { className: "ws-head-row" },
      el(
        "button",
        { type: "button", className: "ws-head", onclick: toggle, "aria-expanded": String(!collapsed) },
        el("span", { className: "chev" }, icon("chev")),
        el("span", { className: "ws-name", textContent: workspace.name, title: workspace.name }),
        rollupChip(workspace),
        launcher.workspaceChip(workspace.id),
        el(
          "span",
          { className: "ws-count", title: `${live} running of ${sessions.length}` },
          live ? el("b", { textContent: `${live}` }) : null,
          live ? "/" : "",
          `${sessions.length}`,
        ),
      ),
      el(
        "span",
        { className: "ws-tools" },
        iconButton("plus", "New session here", () => openSessionDialog(workspace.id)),
        broadcast.workspaceTool(workspace, iconButton),
        iconButton("gear", "Edit workspace", () => openWorkspaceDialog(workspace)),
        iconButton("x", "Delete workspace", () => deleteWorkspace(workspace), true),
      ),
    ),
    el(
      "div",
      { className: "ws-body" },
      el(
        "div",
        { className: "ws-repos" },
        workspace.folders.map((f) =>
          flywheel.folderChip(f, el("span", {
            className: f.missing ? "missing" : f.git ? "git" : "plain",
            textContent: folderLabel(f),
            title: f.missing
              ? `${f.path} (missing: moved or deleted?)`
              : f.git ? `${f.path} (git repo)` : `${f.path} (plain folder, no git)`,
          }), workspace.id)),
      ),
      error
        ? el("p", {
          className: "ws-error",
          role: "alert",
          textContent: error,
          onclick: () => {
            delete state.wsErrors[workspace.id];
            render();
          },
        })
        : null,
      sessions.length
        ? el(
          "ul",
          { className: "sessions" },
          listed.map((s) => {
            const row = renderSession(s, numbering.get(s.id));
            // Its agent is listed in Needs you: no connector to a row that
            // is not above it.
            const parent = s.companion_of || s.review_of;
            if (parent && needsYou.has(parent)) row.classList.add("orphan");
            return row;
          }),
          elsewhere.length ? needsYouLine(elsewhere) : null,
        )
        : el(
          "div",
          { className: "ws-empty" },
          "No sessions. ",
          el("button", {
            type: "button",
            className: "text-btn",
            textContent: "start one",
            onclick: () => openSessionDialog(workspace.id),
          }),
        ),
    ),
  );
}

// With machines: this hub's workspaces, then each machine's, each group
// under a head with its connection state.
function machineGroups(numbering, needsYou) {
  let i = 0;
  const group = (name, list, empty) => el(
    "section",
    { className: `machine-group${name ? "" : " local"}`, dataset: { machine: name || "" } },
    machines.header(name),
    list.length
      ? list.map((w) => renderWorkspace(w, i++, numbering, needsYou))
      : el("div", { className: "ws-empty machine-empty", textContent: empty }),
  );
  const local = state.workspaces.filter((w) => !w.machine);
  return [
    group(null, local, "No workspaces on this machine."),
    ...machines.names().map((name) => {
      const list = state.workspaces.filter((w) => w.machine === name);
      const link = machines.linkOf(name);
      return group(name, list, link === "online" ? "No workspaces there." : link === "connecting" ? "Connecting…" : "Not reachable yet.");
    }),
  ];
}

function render() {
  search.refreshFilters();
  const numbering = new Map(attachable().map((s, i) => [s.id, i]));
  const needsYou = listedInNeedsYou();
  const root = $("#workspaces");
  if (machines.any()) {
    root.replaceChildren(...machineGroups(numbering, needsYou));
  } else if (state.workspaces.length === 0) {
    root.replaceChildren(
      el(
        "div",
        { className: "no-workspaces" },
        "A workspace is a name and the folders (git repos or plain directories) you work on together. Create or import one to start sessions.",
      ),
    );
  } else {
    root.replaceChildren(...state.workspaces.map((w, i) => renderWorkspace(w, i, numbering, needsYou)));
  }
  // The workspace animation plays once, on first paint only.
  root.querySelectorAll(".workspace").forEach((node) => {
    if (state.painted) node.style.animation = "none";
  });
  state.painted = true;

  const running = state.sessions.filter(runningNow).length;
  const tach = $("#tach");
  tach.textContent = `${pad2(running)} live`;
  tach.classList.toggle("on", running > 0);
  const wheel = $("#wheel");
  wheel.style.setProperty("--spin", running ? `${Math.max(1.1, 9 / running)}s` : "0s");
  wheel.style.setProperty("--spin-state", running ? "running" : "paused");

  attention.render();
  launcher.render();
  broadcast.render();
  if (!tabRenaming) renderTabs();
  for (const pane of livePanes()) renderPaneHead(pane);
  renderMain();
}

// The header's agent / companion-shell flip (Alt+`, which phones lack).
function renderFlip(session) {
  const button = $("#flip-toggle");
  const parent = session && session.companion_of ? sessionById(session.companion_of) : null;
  const agent = isAgent(session);
  button.hidden = !parent && !agent;
  if (button.hidden) return;
  const running = agent ? state.sessions.find((s) => s.companion_of === session.id && s.state === "running") : null;
  button.querySelector("use").setAttribute("href", parent ? `#i-${parent.kind}` : "#i-shell");
  $("#flip-label").textContent = parent ? "agent" : "shell";
  // On the companion shell: the agent's status rides on the way back
  // (it can turn blocked while you are in its shell).
  const agentState = parent ? statusLook(parent) : null;
  let dot = button.querySelector(".flip-dot");
  if (agentState && !dot) dot = button.appendChild(el("span", { className: "flip-dot", "aria-hidden": "true" }));
  if (dot && !agentState) dot.remove();
  if (dot && agentState) dot.className = `flip-dot dot ${agentState.dot}`;
  button.title = parent
    ? withKey(`Back to agent ${parent.name}${agentState && agentState.label ? `, ${agentState.label}` : ""}`, "companion.flip")
    : running
      ? withKey(`Companion shell ${running.name}`, "companion.flip")
      : withKey(`Open a companion shell in ${session.name}'s directory`, "companion.flip");
  button.setAttribute("aria-label", button.title);
}

// The header's "to agent" on a shell: its last output, to an agent (its
// own, for a companion shell), editable first.
function renderSendOutput(session) {
  const button = $("#out-toggle");
  const target = outputTarget(session);
  button.hidden = !target;
  if (!target) return;
  $("#out-label").textContent = target.label;
  button.title = withKey(`Send the last command's output to ${target.name} (editable first)`, "handoff.output");
  button.setAttribute("aria-label", button.title);
}

function renderMain() {
  const session = sessionById(state.attached);
  // An empty tab of a workspace that is open (a new tab, its sessions
  // closed): the stage offers that workspace's sessions.
  const emptyTab = !session && !phone() && state.view && workspaceById(state.view) && activeTab() ? focusedLeaf() : null;
  // A board tab fills the stage itself.
  const onBoard = !session && !phone() && !!state.view && L.isBoard((activeTab() || {}).root);
  const workspace = session ? workspaceById(session.workspace_id) : emptyTab || onBoard ? workspaceById(state.view) : null;
  $("#empty").hidden = !!session || onBoard;
  $("#crumb-workspace").textContent = workspace ? workspace.name : "";
  // Its own element: it stays when a phone hides the workspace name.
  const onMachine = workspace && workspace.machine;
  $("#crumb-machine").hidden = !onMachine;
  $("#crumb-machine").replaceChildren(...(onMachine ? [machineBadge(workspace.machine)] : []));
  // A hub with nothing yet: the start screen, not a missing session.
  const nothingYet = !session && !emptyTab && !state.workspaces.length && !machines.any();
  $("#crumb-session").textContent = session ? session.name : emptyTab ? "Empty pane" : onBoard ? "Board" : nothingYet ? "Get started" : "No session";
  // Without a session the header's session controls mean nothing.
  $("#cmd-toggle").hidden = !session;
  $("#link").hidden = !session;
  const kind = $("#crumb-kind");
  kind.hidden = !session;
  if (session) {
    // A restore that could not bring it back as itself runs a shell.
    kind.textContent = window.FwHubFmt.kindLabel(session);
    kind.className = `kind-tag ${session.kind || "shell"}`;
  }
  // Cut from its start (the .path-tail styling).
  $("#meter-cwd").textContent = session ? window.FwHubFmt.pathTail(session.cwd) : "";
  $("#meter-cwd").title = session ? session.cwd : "";
  launcher.renderHeader(session);
  sharing.renderHeader(session);
  promptQueue.renderHeader(session);
  work.renderHeader();
  renderFlip(session);
  renderSendOutput(session);
  updateMeters();
  const counts = attentionCounts();
  document.title = `${titlePrefix(counts)}${session ? `${session.name} · flywheel hub` : "flywheel hub"}`;
  drawFavicon(counts);
  // The phone's only view of other sessions: a badge on the drawer
  // button for the agents elsewhere that need you.
  const elsewhere = attentionCounts(session ? session.id : null);
  const badge = $("#menu-badge");
  const urgent = elsewhere.blocked ? "blocked" : elsewhere.done ? "done" : null;
  badge.hidden = !urgent;
  if (urgent) {
    badge.className = `menu-badge ${urgent}`;
    badge.textContent = String(elsewhere.blocked || elsewhere.done);
  }
  $(".menu-toggle").setAttribute(
    "aria-label",
    urgent ? `Show sessions (${titlePrefix(elsewhere).slice(1, -2)} elsewhere)` : "Show sessions",
  );
  // An agent whose status is not known yet still gets a pill and why?:
  // that is when the explanation is wanted most.
  const pushed = session && state.agent[session.id];
  const agent = session
    ? agentStatus(session) ||
      (session.kind !== "shell" && session.state === "running" && session.live && reachable(session)
        ? { status: "unknown", detail: "", since: pushed ? pushed.since : "" }
        : null)
    : null;
  const pill = $("#crumb-status");
  pill.hidden = !agent;
  if (agent) {
    pill.textContent = agent.status;
    pill.className = `agent-pill ${agent.status}`;
    pill.title = agent.detail ? `${agent.status}: ${agent.detail}` : agent.status;
  }
  explain.render(session, agent);

  commands.render();
  net.defer("commands", () => commands.prefetch());
  // Copy last reply copies at the key press: the attached agent's replies
  // are kept at hand.
  const replying = replies.agentOf(session);
  if (replying) net.defer("replies", () => replies.ensure(replying.id));
  flywheel.render();
  review.render();
  mobile.render();

  const actions = $("#empty-actions");
  // No workspace yet (and no machine's): the first run's cards.
  let first = false;
  if (!session && emptyTab && !state.notice) {
    const others = state.sessions.filter((s) => s.workspace_id === workspace.id && s.live);
    $("#empty-title").textContent = "Empty pane.";
    $("#empty-body").textContent = others.length
      ? `Show one of ${workspace.name}'s sessions here, drag one in from the sidebar, or start a shell.`
      : `Start a shell in ${workspace.name}, or drag a session in from the sidebar.`;
    actions.replaceChildren(
      el("button", {
        type: "button",
        className: "slab",
        onclick: () => createSession({ workspace_id: workspace.id, kind: "shell" }, { pane: emptyTab.id })
          .catch((error) => commands.toast(error.message, { timeout: 5000 })),
      }, icon("plus"), "New shell"),
      ...others.slice(0, 8).map((other) => el("button", {
        type: "button",
        className: "slab ghost",
        onclick: () => openSession(other.id, { pane: emptyTab.id }),
      }, other.name)),
    );
  } else if (!session) {
    const hasWorkspace = state.workspaces.length > 0;
    first = !hasWorkspace && !machines.any();
    // Sessions running but none on screen (a phone's start screen, a
    // fresh browser): say so, not "nothing".
    const running = state.sessions.filter(runningNow).length;
    const narrow = window.matchMedia(NARROW).matches;
    $("#empty-title").textContent = state.noticeTitle ||
      (!hasWorkspace ? "Set up a workspace." : running ? `${running} session${running === 1 ? "" : "s"} running.` : "Nothing on the wheel.");
    $("#empty-body").textContent = state.notice || (hasWorkspace
      ? (running
        ? (narrow
          ? "Open the menu to pick one, or launch a new shell, Claude Code, or Codex session."
          : "Pick one from the left, or launch a new shell, Claude Code, or Codex session.")
        : narrow
          ? "Open the menu for every session, or launch a shell, Claude Code, or Codex in one of your folders."
          : "Pick a session from the left, or launch a shell, Claude Code, or Codex in one of your folders.")
      : "Group the folders you work on together (or import a VS Code workspace), then launch shells and agents inside them. Every terminal lives on the hub: close the tab, come back from your phone, it is still there.");
    actions.replaceChildren(
      ...[hasWorkspace
        ? el("button", { type: "button", className: "slab", onclick: () => openSessionDialog() }, icon("plus"), "New session")
        : null,
      el(
        "button",
        { type: "button", className: `slab${hasWorkspace ? " ghost" : ""}`, onclick: () => openWorkspaceDialog() },
        icon("plus"),
        "New workspace",
      )].filter(Boolean),
    );
  }
  $("#empty").classList.toggle("first", first);
  $("#first-run").hidden = !first;
  if (first) onboarding.render($("#first-run"));
}

function updateMeters() {
  const session = sessionById(state.attached);
  const term = focusedTerm();
  $("#meter-size").textContent = session && term ? `${term.cols}×${term.rows}` : "";
  const viewers = session && session.live ? session.live.clients : 0;
  const links = session && session.live ? session.live.link_viewers || 0 : 0;
  // Link viewers are the share pill's count.
  $("#meter-viewers").textContent = viewers > 1 ? `${viewers} viewers` : "";
  $("#meter-viewers").title = links ? `${links} more through a share link` : "";
}

function showError(error) {
  showBanner(`Error: ${error.message}`, [], false);
}

// The phone footer's More row (app-phone.css): it unfolds the utilities;
// picking one folds them again, except the theme and alerts toggles,
// which are tapped through their states.
function setMore(open) {
  $("#side-foot").classList.toggle("more-open", open);
  $("#side-more-toggle").setAttribute("aria-expanded", String(open));
  if (open) $("#side-foot").scrollIntoView({ block: "end", behavior: "smooth" });
}
$("#side-more-toggle").addEventListener("click", () => setMore(!$("#side-foot").classList.contains("more-open")));
$("#side-more").addEventListener("click", (event) => {
  const picked = event.target.closest("button");
  if (picked && picked.dataset.action !== "cycle-theme" && picked.id !== "alerts-toggle") setMore(false);
});

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
  revealInList(session);
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

// What a row says at its right end, short: an agent that needs you
// (blocked, asking), a finished turn, a plan's count; a shell or an idle
// agent says nothing (its dot does), an ended one how it ended.
function rowMeta(session, st) {
  const agent = agentStatus(session);
  if (agent) {
    if (agent.status === "working") {
      const t = sessionTelemetry.get(session.id);
      return t && t.plan && t.plan.total ? `${t.plan.done}/${t.plan.total}` : "";
    }
    if (agent.status === "idle") return "";
    if (agent.status === "done") return agent.since ? `done ${age(agent.since)}` : "done";
    return st.label;
  }
  return st.dot === "live" ? "" : st.label;
}

// `opts.tag`: the All list's rows carry their workspace's rail initials;
// `opts.flat`: no companion connector (its agent may sit in another group).
function renderSession(session, opts = {}) {
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
  const workspace = opts.tag ? workspaceById(session.workspace_id) : null;
  const parent = opts.flat ? null : session.companion_of || session.review_of;
  return el(
    "li",
    {
      className: `session${active ? " active" : ""}${shown ? " shown" : ""}${session.live ? "" : " gone"}${st.dot === "blocked" ? " needs-you" : ""}${parent ? " companion" : ""}${broadcast.isPicked(session.id) ? " picked" : ""}`,
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
    // The kind's glyph wears the status dot on its corner. Restored as a
    // plain shell: the agent's glyph, dimmed, says so.
    el(
      "span",
      {
        className: `glyph ${glyphFor(session.kind)}${session.fallback_command ? " as-shell" : ""}`,
        title: `${window.FwHubFmt.kindLabel(session)}: ${st.title || st.label}`,
      },
      icon(glyphFor(session.kind)),
      el("span", { className: `dot ${st.dot}`, "aria-hidden": "true" }),
    ),
    nameNode,
    el(
      "span",
      { className: "session-meta" },
      // A workspace's list has no machine badge: its tile says where.
      opts.tag && session.machine ? machineBadge(session.machine) : null,
      recBadge(session),
      launcher.rowChip(session),
      promptQueue.rowChip(session),
      pullRequests.rowChip(session),
      el("span", {
        className: `session-state ${st.tone}`,
        textContent: rowMeta(session, st),
        title: st.title || st.label,
      }),
      workspace ? el("span", { className: "ws-tag", textContent: initials(workspace.name), title: workspace.name }) : null,
      actions,
    ),
    // What a blocked agent asks, readable without hover (touch).
    st.dot === "blocked" && st.detail
      ? el("span", { className: "session-detail", textContent: st.detail })
      : null,
    progressLine(session, st),
    // Where it runs (shown on the active row); a companion (or a
    // reviewer) under its agent: the agent's line says it all.
    parent && sessionById(parent) ? null : whereLine(session),
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
    const bar = el("span", { className: "prog-bar" }, el("i"));
    bar.firstChild.style.width = `${Math.round((t.plan.done / t.plan.total) * 100)}%`;
    parts.push(bar);
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

// --- the rail and the list --------------------------------------------

// A workspace's rail initials: the first letters of its first two words
// ("infrared-rust" -> IR), else its first two letters.
function initials(name) {
  const words = String(name || "").split(/[\s_\-./]+/).filter(Boolean);
  const out = words.length > 1 ? words[0][0] + words[1][0] : (words[0] || "?").slice(0, 2);
  return out.toUpperCase();
}

// An agent whose turn ended asking you something (telemetry's `asking`).
function askingNow(session) {
  const t = sessionTelemetry.get(session.id);
  return !!(t && t.status === "asking" && t.ask);
}

// What agents need, in one workspace (or all, `null`), counted as Needs
// you counts them: blocked or asking; finished turns nobody here has
// looked at (an open fan-out's candidates are its one row); working.
function workspaceNeeds(workspaceId) {
  const graded = fanout.candidateSessions();
  const out = { blocked: 0, done: 0, working: 0 };
  for (const session of state.sessions) {
    if (workspaceId && session.workspace_id !== workspaceId) continue;
    const agent = agentStatus(session);
    if (!agent) continue;
    if (agent.status === "blocked" || (agent.status === "done" && askingNow(session))) out.blocked += 1;
    else if (agent.status === "done" && !graded.has(session.id)) out.done += 1;
    else if (agent.status === "working") out.working += 1;
  }
  out.done += fanout.attention().filter((f) => !workspaceId || f.workspace_id === workspaceId).length;
  return out;
}

function needsText({ blocked, done, working }) {
  return [
    blocked ? `${blocked} need${blocked === 1 ? "s" : ""} you` : "",
    done ? `${done} done` : "",
    working ? `${working} working` : "",
  ].filter(Boolean).join(", ");
}

// A tile's marker: a count of what needs you, else a pip for a finished
// turn, else one for work under way.
function railMarker(needs, badgeId) {
  if (needs.blocked) return el("span", { className: "rail-badge", id: badgeId, textContent: String(needs.blocked) });
  if (needs.done) return el("span", { className: "rail-pip done", id: badgeId });
  if (needs.working) return el("span", { className: "rail-pip working", id: badgeId });
  return null;
}

// The workspace the list shows, or null for Everything. A scope whose
// workspace is not listed (yet: a machine's lists load later) reads as
// Everything without being forgotten.
function scopedWorkspace() {
  return state.scope === "all" ? null : workspaceById(state.scope) || null;
}

// A tile picked: its list. The picked one again folds the list away (a
// desktop's rail alone), and back.
function pickScope(scope) {
  if (state.scope === scope && !window.matchMedia(NARROW).matches) {
    state.listHidden = !state.listHidden;
  } else {
    state.scope = scope;
    state.listHidden = false;
  }
  store.set("scope", state.scope);
  store.set("list-hidden", state.listHidden);
  render();
}

// A session about to be acted on in the list (a rename): its row on
// screen -- its workspace's list, or Everything with Resting unfolded.
function revealInList(session) {
  if (state.scope !== "all" && state.scope !== session.workspace_id) {
    state.scope = session.workspace_id;
    store.set("scope", state.scope);
  }
  if (state.scope === "all" && !state.restingOpen) {
    state.restingOpen = true;
    store.set("resting-open", true);
  }
  if (state.listHidden) {
    state.listHidden = false;
    store.set("list-hidden", false);
  }
}

// A session opened from elsewhere (Alt 1-9, the palette, a link) while
// the list shows another workspace: the list follows it there.
function followAttached() {
  const session = sessionById(state.attached);
  if (!session || state.scopeFollows === session.id) return;
  state.scopeFollows = session.id;
  if (state.scope !== "all" && state.scope !== session.workspace_id && workspaceById(session.workspace_id)) {
    state.scope = session.workspace_id;
    store.set("scope", state.scope);
  }
}

function railTile(workspace) {
  const needs = workspaceNeeds(workspace.id);
  const picked = state.scope === workspace.id;
  const here = sessionById(state.attached)?.workspace_id === workspace.id;
  const said = needsText(needs);
  const label = `${workspace.name}${workspace.machine ? ` on ${workspace.machine}` : ""}${said ? `: ${said}` : ""}`;
  return el(
    "button",
    {
      type: "button",
      className: `rail-tile ws${picked ? " on" : ""}${here ? " here" : ""}`,
      dataset: { id: workspace.id },
      title: picked && !window.matchMedia(NARROW).matches ? `${label} (again: ${state.listHidden ? "show" : "hide"} the list)` : label,
      "aria-label": label,
      "aria-current": picked ? "true" : undefined,
      onclick: () => pickScope(workspace.id),
    },
    el("span", { className: "rail-initials", textContent: initials(workspace.name) }),
    railMarker(needs),
  );
}

// This hub's workspaces, then each machine's under its own mark (its
// link's colour; a tap opens Machines).
function renderRail() {
  const tiles = state.workspaces.filter((w) => !w.machine).map(railTile);
  for (const name of machines.names()) {
    const link = machines.linkOf(name) || "offline";
    tiles.push(
      el("button", {
        type: "button",
        className: `rail-machine link-${link}`,
        textContent: initials(name),
        title: `machine ${name}: ${link}`,
        "aria-label": `machine ${name}: ${link}`,
        onclick: () => machines.openDialog(),
      }),
      ...state.workspaces.filter((w) => w.machine === name).map(railTile),
    );
  }
  $("#rail-tiles").replaceChildren(...tiles);
  const all = $("#rail-all");
  const needs = workspaceNeeds(null);
  const picked = state.scope === "all" || !scopedWorkspace();
  all.classList.toggle("on", picked);
  if (picked) all.setAttribute("aria-current", "true");
  else all.removeAttribute("aria-current");
  const said = needsText(needs);
  all.title = `Everything${said ? `: ${said}` : ": what needs you, what is moving, the rest"}`;
  all.setAttribute("aria-label", all.title);
  $("#rail-all-badge")?.remove();
  const marker = railMarker(needs, "rail-all-badge");
  if (marker) all.append(marker);
}

function renderListHead(workspace) {
  const title = $("#list-title");
  title.textContent = workspace ? workspace.name : "Flywheel";
  title.title = workspace ? workspace.name : "";
  title.classList.toggle("brand-title", !workspace);
  $("#tach").hidden = !!workspace;
  const sub = $("#list-sub");
  if (workspace) {
    const here = state.sessions.filter((s) => s.workspace_id === workspace.id);
    const live = here.filter((s) => s.live && s.state === "running" && reachable(s)).length;
    sub.replaceChildren(
      el("span", { className: "ws-count", title: `${live} running of ${here.length}` }, live ? el("b", { textContent: `${live}` }) : null, live ? "/" : "", `${here.length} session${here.length === 1 ? "" : "s"}`),
      launcher.workspaceChip(workspace.id) || "",
    );
  } else {
    sub.replaceChildren();
  }
  $("#list-tools").replaceChildren(...(workspace
    ? [
      broadcast.workspaceTool(workspace, iconButton),
      iconButton("gear", "Edit workspace", () => openWorkspaceDialog(workspace)),
      iconButton("x", "Delete workspace", () => deleteWorkspace(workspace), true),
    ].filter(Boolean)
    : []));
  const add = $("#list-new");
  add.title = withKey(workspace ? `New session in ${workspace.name}` : "New session", "session.new");
  add.setAttribute("aria-label", add.title);
}

// A small heading inside the list.
const listGroup = (name, count) =>
  el("div", { className: "list-group" }, el("span", { className: "list-group-name", textContent: name }), el("span", { className: "list-group-n", textContent: String(count) }));

// Everything: Needs you (attention.js, above this list) first, then the
// agents at work, then the rest folded into one line. A session is listed
// once, in its most urgent group.
// An agent that stops working stays in Moving this long: a pause between
// two steps does not bounce its row to Resting and back.
const MOVING_HOLD_MS = 20000;
const lastWorking = new Map(); // session id -> ms it was last seen working
let movingTimer = 0;

function renderAllList() {
  // What Needs you shows above (held while a reply is typed there).
  const needs = attention.shownIds();
  const moving = [];
  const resting = [];
  const now = Date.now();
  let nextExpiry = Infinity;
  for (const session of orderedSessions()) {
    // A turn that ended went to Needs you: no hold after it is seen.
    if (needs.has(session.id)) {
      lastWorking.delete(session.id);
      continue;
    }
    const agent = agentStatus(session);
    if (agent && agent.status === "working") {
      lastWorking.set(session.id, now);
      moving.push(session);
      continue;
    }
    const until = (lastWorking.get(session.id) || 0) + MOVING_HOLD_MS;
    if (session.live && until > now) {
      moving.push(session);
      nextExpiry = Math.min(nextExpiry, until);
    } else {
      lastWorking.delete(session.id);
      resting.push(session);
    }
  }
  // Settle a held row once its hold ends.
  clearTimeout(movingTimer);
  if (nextExpiry !== Infinity) movingTimer = setTimeout(() => render(), nextExpiry - now + 50);
  const rows = (list) => el("ul", { className: "sessions" }, list.map((s) => renderSession(s, { tag: true, flat: true })));
  const out = [];
  if (!state.sessions.length) {
    out.push(el(
      "div",
      { className: "ws-empty" },
      "No sessions yet. ",
      el("button", { type: "button", className: "text-btn", textContent: "start one", onclick: () => openSessionDialog() }),
    ));
    return out;
  }
  if (!needs.size && !moving.length) {
    out.push(el("div", { className: "list-calm", textContent: "Nothing needs you. Nothing is moving." }));
  }
  if (moving.length) out.push(listGroup("Moving", moving.length), rows(moving));
  if (resting.length) {
    const open = state.restingOpen;
    out.push(el(
      "button",
      {
        type: "button",
        className: `list-group fold${open ? " open" : ""}`,
        "aria-expanded": String(open),
        title: open ? "Fold the resting sessions" : "Show the resting sessions",
        onclick: () => {
          state.restingOpen = !open;
          store.set("resting-open", state.restingOpen);
          render();
        },
      },
      el("span", { className: "chev" }, icon("chev")),
      el("span", { className: "list-group-name", textContent: "Resting" }),
      el("span", { className: "list-group-n", textContent: String(resting.length) }),
      open ? null : el("span", { className: "list-group-names", textContent: resting.map((s) => s.name).join(", ") }),
    ));
    // Folded, the attached session still shows.
    const shown = open ? resting : resting.filter((s) => s.id === state.attached);
    if (shown.length) out.push(rows(shown));
  }
  return out;
}

// One workspace: its machine (when another's), a way back when agents
// elsewhere need you, its folders, its sessions.
function renderWorkspaceList(workspace) {
  const sessions = state.sessions.filter((s) => s.workspace_id === workspace.id);
  const error = state.wsErrors[workspace.id];
  const elsewhere = attention.entries().filter((e) => e.level < 2 && e.session.workspace_id !== workspace.id).length;
  return [
    workspace.machine ? machines.header(workspace.machine) : null,
    elsewhere
      ? el(
        "button",
        { type: "button", className: "list-elsewhere", title: "Show Everything", onclick: () => pickScope("all") },
        el("span", { className: "dot blocked", "aria-hidden": "true" }),
        `${elsewhere} need${elsewhere === 1 ? "s" : ""} you elsewhere`,
      )
      : null,
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
      ? el("ul", { className: "sessions" }, sessions.map((s) => renderSession(s)))
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
  ].filter(Boolean);
}

function render() {
  // An inline rename being typed is never torn down under the user, by
  // any repaint (a status push, a timer, a refresh): rebuilding the list
  // would drop the field's focus, and its blur ends the rename. The list
  // repaints when the rename ends (renameSession).
  if (state.renaming && $(".session .rename")) {
    attention.render();
    renderMain();
    return;
  }
  search.refreshFilters();
  followAttached();
  const workspace = scopedWorkspace();
  document.body.classList.toggle("list-hidden", state.listHidden && !window.matchMedia(NARROW).matches);
  $("#sidebar").classList.toggle("scope-ws", !!workspace);
  renderRail();
  renderListHead(workspace);
  // Needs you first: the Everything list below leaves out what it shows.
  attention.render();
  const root = $("#workspaces");
  root.replaceChildren(...(!state.workspaces.length && !machines.any()
    ? [el(
      "div",
      { className: "no-workspaces" },
      "A workspace is a name and the folders (git repos or plain directories) you work on together. Create or import one to start sessions.",
    )]
    : workspace ? renderWorkspaceList(workspace) : renderAllList()));

  const running = state.sessions.filter(runningNow).length;
  const tach = $("#tach");
  tach.textContent = `${pad2(running)} live`;
  tach.classList.toggle("on", running > 0);
  const wheel = $("#wheel");
  wheel.style.setProperty("--spin", running ? `${Math.max(1.1, 9 / running)}s` : "0s");
  wheel.style.setProperty("--spin-state", running ? "running" : "paused");

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
  const workspace = session ? workspaceById(session.workspace_id) : emptyTab ? workspaceById(state.view) : null;
  $("#empty").hidden = !!session;
  $("#crumb-workspace").textContent = workspace ? workspace.name : "";
  // Its own element: it stays when a phone hides the workspace name.
  const onMachine = workspace && workspace.machine;
  $("#crumb-machine").hidden = !onMachine;
  $("#crumb-machine").replaceChildren(...(onMachine ? [machineBadge(workspace.machine)] : []));
  // A hub with nothing yet: the start screen, not a missing session.
  const nothingYet = !session && !emptyTab && !state.workspaces.length && !machines.any();
  $("#crumb-session").textContent = session ? session.name : emptyTab ? "Empty pane" : nothingYet ? "Get started" : "No session";
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

// The rail's More: the rarer actions and the utilities, in a popover off
// the rail. Picking one closes it, except the theme and alerts toggles,
// which are tapped through their states.
const moreOpen = () => $("#side-foot").classList.contains("more-open");
function setMore(open) {
  $("#side-foot").classList.toggle("more-open", open);
  $("#side-more-toggle").setAttribute("aria-expanded", String(open));
}
$("#side-more-toggle").addEventListener("click", () => setMore(!moreOpen()));
$("#side-more").addEventListener("click", (event) => {
  const picked = event.target.closest("button");
  if (picked && picked.dataset.action !== "cycle-theme" && picked.id !== "alerts-toggle") setMore(false);
});
document.addEventListener("click", (event) => {
  if (moreOpen() && !event.target.closest("#side-foot, #side-more-toggle")) setMore(false);
});
document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape" || !moreOpen()) return;
  event.stopPropagation();
  setMore(false);
  $("#side-more-toggle").focus();
}, true);
$("#rail-all").addEventListener("click", () => pickScope("all"));
$("#list-new").addEventListener("click", () => openSessionDialog(scopedWorkspace()?.id));

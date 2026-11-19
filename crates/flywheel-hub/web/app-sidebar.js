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
  // An agent, or a session in a worktree of its own: the close checklist,
  // run as `fw hub close` runs it.
  if ((isAgent(session) || session.worktree) && !session.companion_of && !session.review_of) {
    const how = await worktrees.closeChecklist(session);
    if (!how) return;
    if (how.closed) {
      if (how.text) commands.toast(`Closed ${session.name}: ${how.text}`, { timeout: 9000 });
      if (!how.forget) return refresh();
    } else if (how.kill && how.remove) {
      query = "?remove_worktree=true";
    }
  } else if (session.worktree && !session.companion_of && !session.review_of) {
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

// A row says one thing at its right end: what its dot alone cannot.
// Working: its plan's count, else how long its step has run; background:
// how many agents still run; quiet: for how long; resting: since when;
// ended: how. Everything else is the inspector's (app-inspector.js).
function rowFact(session, st) {
  if (session.ending) return { text: "ending", tone: "brass" };
  const agent = agentStatus(session);
  if (agent) {
    if (st.quiet) return { text: st.label, tone: "quiet" };
    if (st.dot === "handed") return { text: "handed off", tone: "" };
    const t = sessionTelemetry.get(session.id);
    const doing = doingOf(session, agent);
    switch (agent.state) {
      case "working":
      case "compacting":
        if (t && t.plan && t.plan.total) return { text: `${t.plan.done}/${t.plan.total}`, tone: "" };
        return { text: (doing && doing.age) || "", tone: "" };
      case "background":
        return { text: (doing && doing.count) || "background", tone: "" };
      case "idle":
        return { text: agent.since ? age(agent.since) : "", tone: "" };
      case "done":
        return { text: "done", tone: "brass" };
      default:
        return { text: st.label, tone: st.tone };
    }
  }
  return { text: st.dot === "live" ? "" : st.label, tone: st.tone };
}

// The one line a row adds under its name, only when it is acted on: what
// a blocked agent waits on, a failed turn's error, a context window
// nearly full.
const CTX_WARN = 85;
function rowAlert(session, st) {
  const t = sessionTelemetry.get(session.id);
  if (st.dot === "blocked" && st.detail) return el("span", { className: "session-detail", textContent: st.detail });
  const agent = agentStatus(session);
  if (agent && agent.state === "failed") {
    return el("span", { className: "session-detail", textContent: `failed: ${(t && t.last_error) || st.detail || "the turn ended on an error"}` });
  }
  if (agent && agent.status === "working" && t && t.context && t.context.pct >= CTX_WARN) {
    const pct = `${t.context.exact === false ? "~" : ""}${Math.round(t.context.pct)}%`;
    return el("span", { className: "session-detail session-ctx", title: "context window used" }, `ctx ${pct}`, el("span", { className: "muted", textContent: " · compacts soon" }));
  }
  return null;
}

// A family's rollup on its lead's row: what its children are, at most
// three glyphs with counts (needs you, moving, quiet).
const ROLL_NEEDS = ["blocked", "asking", "bad"];
function rollup(children) {
  const looks = children.map((c) => statusLook(c));
  const needs = looks.filter((l) => ROLL_NEEDS.includes(l.dot));
  const moving = looks.filter((l) => ["working", "compacting", "background"].includes(l.dot)).length;
  const quiet = looks.filter((l) => l.dot === "quiet").length;
  const parts = [];
  const said = [];
  if (needs.length) {
    const worst = ROLL_NEEDS.find((d) => needs.some((l) => l.dot === d));
    parts.push(el("span", { className: `dot ${worst}`, "aria-hidden": "true" }), String(needs.length));
    said.push(`${needs.length} need${needs.length === 1 ? "s" : ""} you`);
  }
  if (moving) {
    parts.push(el("span", { className: "dot working", "aria-hidden": "true" }), String(moving));
    said.push(`${moving} moving`);
  }
  if (quiet) {
    parts.push(el("span", { className: "dot quiet", "aria-hidden": "true" }), String(quiet));
    said.push(`${quiet} quiet`);
  }
  if (!parts.length) return null;
  return el("span", { className: "session-roll", title: `its sessions: ${said.join(", ")}` }, parts);
}

// `opts.kids`: the sessions this one started (a lead's children, its
// companion shell, its reviewer), listed under it; `opts.child`: it is
// one of them.
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
    : el("span", { className: "session-name", textContent: session.name });
  const actions = el(
    "span",
    { className: "session-actions" },
    iconButton("dots", "Details (I)", () => inspector.toggle(session.id)),
    Object.assign(iconButton("pen", "Rename (F2)", () => startRename(session.id)), { className: "icon rename-action" }),
    session.state === "running" && session.live
      ? iconButton("stop", "Stop: hang up, keep the scrollback", () => stopSession(session.id))
      : iconButton("redo", "Relaunch in place", () => relaunch(session.id)),
    iconButton("x", "Close session", () => closeSession(session.id), true),
  );
  const fact = rowFact(session, st);
  const kids = opts.kids || [];
  const roll = kids.length ? rollup(kids) : null;
  const kind = window.FwHubFmt.kindLabel(session);
  return el(
    "li",
    {
      className: `session${active ? " active" : ""}${shown ? " shown" : ""}${session.live ? "" : " gone"}${st.dot === "blocked" ? " needs-you" : ""}${opts.child ? " child" : ""}${kids.length ? " lead" : ""}${broadcast.isPicked(session.id) ? " picked" : ""}`,
      dataset: { id: session.id },
      tabIndex: 0,
      role: "button",
      "aria-current": active ? "true" : undefined,
      "aria-label": `${session.name}, ${kind}, ${st.title || st.label}${fact.text ? `, ${fact.text}` : ""}`,
      // Drag onto a pane: its edge splits that way, its middle shows it there.
      draggable: !window.matchMedia(NARROW).matches,
      ondragstart: (e) => {
        dragged = session.id;
        inspector.close();
        e.dataTransfer.setData(DRAG_TYPE, session.id);
        e.dataTransfer.setData("text/plain", session.name);
        e.dataTransfer.effectAllowed = "copyMove";
      },
      // The inspector floats beside the row it is over (never in the
      // list: no row moves for it).
      onmouseenter: (e) => inspector.hover(session.id, e.currentTarget),
      onmouseleave: () => inspector.leave(),
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
        } else if (e.key === "i" || e.key === "I") {
          e.preventDefault();
          inspector.toggle(session.id, e.currentTarget);
        } else if (e.key === "Escape" && inspector.isOpen()) {
          e.preventDefault();
          inspector.close();
        }
      },
    },
    // One state channel: the dot. Its kind is in the title and the
    // inspector; a restored-as-shell agent's dot is dimmed.
    el(
      "span",
      { className: `glyph state${session.fallback_command ? " as-shell" : ""}`, title: `${kind}: ${st.title || st.label}` },
      el("span", { className: `dot ${st.dot}`, "aria-hidden": "true" }),
    ),
    nameNode,
    el(
      "span",
      { className: "session-meta" },
      recBadge(session),
      roll || el("span", { className: `session-state ${fact.tone}`, textContent: fact.text, title: st.title || st.label }),
      actions,
    ),
    rowAlert(session, st),
  );
}

// What the Everything list may be grouped by: state (its own groups:
// Needs you, Moving, Resting), workspace, lead, or a label key any
// session holds (the hub's fw.* keys after the rest). `state.groupBy`:
// "" (state), "@workspace", "@lead", or the label key.
const GROUP_WORKSPACE = "@workspace";
const GROUP_LEAD = "@lead";
function groupKeys() {
  const keys = new Set();
  for (const s of state.sessions) for (const k of Object.keys(s.labels || {})) keys.add(k);
  if (state.groupBy && !state.groupBy.startsWith("@")) keys.add(state.groupBy);
  return [...keys].sort((a, b) => (a.startsWith("fw.") - b.startsWith("fw.")) || a.localeCompare(b));
}

function setGroupBy(key) {
  state.groupBy = key;
  store.set("group-by", key);
  render();
}

// The group-by picker in the list head (Everything only).
function groupByPicker() {
  const keys = groupKeys();
  return el(
    "select",
    {
      className: `group-by${state.groupBy ? " on" : ""}`,
      title: "Group the list by state, workspace, lead, or a label",
      "aria-label": "Group the list by",
      onchange: (e) => setGroupBy(e.target.value),
    },
    el("option", { value: "", textContent: "by state", selected: !state.groupBy }),
    el("option", { value: GROUP_WORKSPACE, textContent: "by workspace", selected: state.groupBy === GROUP_WORKSPACE }),
    el("option", { value: GROUP_LEAD, textContent: "by lead", selected: state.groupBy === GROUP_LEAD }),
    keys.length
      ? el("optgroup", { label: "by label" }, keys.map((k) => el("option", { value: k, textContent: `by ${k}`, selected: k === state.groupBy })))
      : null,
  );
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
  const agent = agentStatus(session);
  return !!(agent && agent.state === "asking");
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
    // How the list is grouped, beside its count: the picker says it and
    // changes it (morphed: an open picker survives a repaint).
    morph(sub, [groupByPicker()]);
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

// --- families: a lead and the sessions it started -------------------------

// The listed session that started this one: its lead (start_session),
// the agent a companion shell belongs to, the author a reviewer reviews.
function parentOf(session, listed) {
  for (const id of [session.opened_by, session.companion_of, session.review_of]) {
    if (id && id !== session.id && listed.has(id)) return id;
  }
  return null;
}

// `sessions` as families, in their order: each root with every session
// under it (a child's own children list under the root too).
function families(sessions) {
  const listed = new Map(sessions.map((s) => [s.id, s]));
  const rootOf = (session) => {
    let at = session;
    const seen = new Set([at.id]);
    for (;;) {
      const up = parentOf(at, listed);
      if (!up || seen.has(up)) return at;
      seen.add(up);
      at = listed.get(up);
    }
  };
  const roots = new Map();
  const of = new Map(sessions.map((s) => [s.id, rootOf(s)]));
  for (const s of sessions) if (of.get(s.id) === s) roots.set(s.id, { root: s, kids: [] });
  for (const s of sessions) if (of.get(s.id) !== s) roots.get(of.get(s.id).id).kids.push(s);
  return [...roots.values()];
}

const endedSession = (s) => s.state !== "running";

// A family's rows: the root, then its children on a guide rule; ended
// children fold into one line (a tap lists them), children Needs you
// shows are one line pointing there.
function familyRows({ root, kids }, needs = new Set()) {
  const ended = kids.filter((k) => !needs.has(k.id) && endedSession(k));
  const waiting = kids.filter((k) => needs.has(k.id));
  const listed = kids.filter((k) => !needs.has(k.id) && !endedSession(k));
  const open = state.endedOpen.has(root.id);
  const rows = [renderSession(root, { kids: kids.filter((k) => !endedSession(k)) })];
  if (!kids.length) return rows;
  const quiet = (key, dot, label, names, title, onclick) =>
    el("li", { className: "session-fold", dataset: { id: `${key}:${root.id}` } },
      el("button", { type: "button", className: "fold-line", title, onclick },
        el("span", { className: `dot ${dot}`, "aria-hidden": "true" }),
        el("b", { textContent: label }),
        el("span", { className: "fold-names", textContent: names })));
  const inner = listed.map((k) => renderSession(k, { child: true }));
  if (waiting.length) {
    inner.push(quiet("needs", "asking", `${waiting.length} in Needs you`, waiting.map((k) => k.name).join(", "),
      "Shown in Needs you above: open the first", () => openSession(waiting[0].id)));
  }
  if (ended.length) {
    inner.push(quiet("ended", "stopped", `${ended.length} ended`, open ? "" : ended.map((k) => k.name).join(", "),
      open ? "Fold the ended sessions" : "Show the ended sessions", () => {
        if (open) state.endedOpen.delete(root.id);
        else state.endedOpen.add(root.id);
        render();
      }));
    if (open) inner.push(...ended.map((k) => renderSession(k, { child: true })));
  }
  if (inner.length) {
    rows.push(el("li", { className: "session-kids", dataset: { id: `kids:${root.id}` } }, el("ul", { className: "sessions kids" }, inner)));
  }
  return rows;
}

// --- the filter -----------------------------------------------------------

// Words a session is found by: its name (every plain word a part of it),
// `ws:NAME` its workspace, `is:STATE` its state as the legend names it
// (blocked, asking, working, quiet, background, done, resting, ended, ...),
// `label:KEY` a label it holds, `label:KEY=VALUE` (a value's start) its value.
// `fw hub ls --filter` reads the same words.
function filterTerms(query) {
  return String(query || "").toLowerCase().split(/\s+/).filter(Boolean);
}
function stateWords(session) {
  const st = statusLook(session);
  const key = legend.keyOf(st);
  const words = [key, st.state, st.label, st.dot].filter(Boolean).map((w) => String(w).toLowerCase());
  if (key === "idle") words.push("resting");
  if (endedSession(session)) words.push("ended");
  return words;
}
function matchesFilter(session, terms) {
  return terms.every((term) => {
    if (term.startsWith("ws:")) {
      const ws = workspaceById(session.workspace_id);
      return !!ws && ws.name.toLowerCase().includes(term.slice(3));
    }
    if (term.startsWith("is:")) return stateWords(session).some((w) => w.startsWith(term.slice(3)));
    if (term.startsWith("label:")) {
      const [key, value] = term.slice(6).split(/=(.*)/s);
      const held = Object.entries(session.labels || {}).find(([k]) => k.toLowerCase() === key);
      return !!held && (value === undefined || String(held[1]).toLowerCase().startsWith(value));
    }
    return session.name.toLowerCase().includes(term);
  });
}

const filterBox = $("#list-filter");
filterBox.addEventListener("input", () => {
  state.filter = filterBox.value;
  render();
});
filterBox.addEventListener("keydown", (event) => {
  event.stopPropagation();
  if (event.key === "Escape") {
    filterBox.value = "";
    state.filter = "";
    render();
    focusTerminal();
  } else if (event.key === "ArrowDown" || event.key === "Enter") {
    // Into the matches: the first row.
    const first = $("#workspaces li.session");
    if (first) {
      event.preventDefault();
      if (event.key === "Enter") first.click();
      else first.focus();
    }
  }
});
// Dense rows: more of a big fleet on screen.
const densityToggle = $("#list-density");
function applyDensity() {
  $("#sidebar").classList.toggle("dense", state.dense);
  densityToggle.setAttribute("aria-pressed", String(state.dense));
  densityToggle.title = state.dense ? "Roomy rows" : "Dense rows";
}
densityToggle.addEventListener("click", () => {
  state.dense = !state.dense;
  store.set("dense", state.dense);
  applyDensity();
});
applyDensity();

// Matches across every list, flat (a family's members each on their own).
function renderMatches(sessions) {
  const terms = filterTerms(state.filter);
  const found = sessions.filter((s) => matchesFilter(s, terms));
  if (!found.length) return [el("div", { className: "list-calm", textContent: `Nothing matches "${state.filter.trim()}".` })];
  return [listGroup("Matches", found.length), el("ul", { className: "sessions", dataset: { id: "group:matches" } }, found.map((s) => renderSession(s)))];
}

// --- Everything -------------------------------------------------------------

// Everything: Needs you (attention.js, above this list) first, then the
// agents at work, then the rest folded into one line. A session is listed
// once, in its most urgent group; a family (a lead and the sessions it
// started) sits together in its most urgent member's group.
// An agent that stops working stays in Moving this long: a pause between
// two steps does not bounce its row to Resting and back.
const MOVING_HOLD_MS = 20000;
const lastWorking = new Map(); // session id -> ms it was last seen working
let movingTimer = 0;

// Newest first: when it came to rest (its status's change, its end).
function restedAt(family) {
  const s = family.root;
  const agent = agentStatus(s);
  return Date.parse((agent && agent.since) || (s.ended && s.ended.at) || s.created_at || 0) || 0;
}

function renderAllList() {
  // What Needs you shows above (held while a reply is typed there).
  const needs = attention.shownIds();
  const now = Date.now();
  let nextExpiry = Infinity;
  const movingNow = (session) => {
    if (needs.has(session.id)) {
      lastWorking.delete(session.id);
      return false;
    }
    const agent = agentStatus(session);
    if (agent && agent.status === "working") {
      lastWorking.set(session.id, now);
      return true;
    }
    const until = (lastWorking.get(session.id) || 0) + MOVING_HOLD_MS;
    if (session.live && until > now) {
      nextExpiry = Math.min(nextExpiry, until);
      return true;
    }
    lastWorking.delete(session.id);
    return false;
  };
  if (!state.sessions.length) {
    return [el(
      "div",
      { className: "ws-empty" },
      "No sessions yet. ",
      el("button", { type: "button", className: "text-btn", textContent: "start one", onclick: () => openSessionDialog() }),
    )];
  }
  if (filterTerms(state.filter).length) return renderMatches(orderedSessions());
  const moving = [];
  const resting = [];
  for (const family of families(orderedSessions())) {
    // A root Needs you shows: its children list on their own.
    const groups = needs.has(family.root.id) ? family.kids.map((k) => ({ root: k, kids: [] })) : [family];
    for (const f of groups) {
      if (needs.has(f.root.id)) continue;
      const members = [f.root, ...f.kids.filter((k) => !needs.has(k.id))];
      // Every member's hold is kept (or ended) whichever group it lands in.
      const flags = members.map(movingNow);
      (flags.some(Boolean) ? moving : resting).push(f);
    }
  }
  // Settle a held row once its hold ends.
  clearTimeout(movingTimer);
  if (nextExpiry !== Infinity) movingTimer = setTimeout(() => backgroundRender(), nextExpiry - now + 50);
  // Keyed by group, so a repaint keeps each group's rows (morph).
  const rows = (list, group) => el("ul", { className: "sessions", dataset: { id: `group:${group}` } }, list.flatMap((f) => familyRows(f, needs)));
  const count = (list) => list.reduce((n, f) => n + 1 + f.kids.filter((k) => !needs.has(k.id) && !endedSession(k)).length, 0);
  if (state.groupBy && state.sessions.length) return groupedList([...moving, ...resting], rows, count);
  const out = [];
  if (!needs.size && !attention.handoffCount() && !moving.length) {
    out.push(el("div", { className: "list-calm", textContent: "Nothing needs you. Nothing is moving." }));
  }
  if (moving.length) {
    const heading = listGroup("Moving", count(moving));
    // What each state means (legend.js), the attached session's marked.
    heading.append(el("button", {
      type: "button",
      className: "list-group-legend",
      dataset: { id: "legend" },
      textContent: "legend",
      title: "What each state means",
      "aria-haspopup": "dialog",
      "aria-expanded": "false",
      onclick: (e) => {
        const attached = sessionById(state.attached);
        legend.toggle(e.currentTarget, attached ? legend.keyOf(statusLook(attached)) : "");
      },
    }));
    out.push(heading, rows(moving, "moving"));
  }
  if (resting.length) {
    resting.sort((a, b) => restedAt(b) - restedAt(a));
    const open = state.restingOpen;
    out.push(el(
      "button",
      {
        type: "button",
        className: `list-group fold${open ? " open" : ""}`,
        "aria-expanded": String(open),
        title: open ? "Fold the resting sessions" : "Show the resting sessions, newest first",
        onclick: () => {
          state.restingOpen = !open;
          store.set("resting-open", state.restingOpen);
          render();
        },
      },
      el("span", { className: "chev" }, icon("chev")),
      el("span", { className: "list-group-name", textContent: "Resting" }),
      el("span", { className: "list-group-n", textContent: String(count(resting)) }),
      open ? null : el("span", { className: "list-group-names", textContent: resting.map((f) => f.root.name).join(", ") }),
    ));
    // Folded, the attached session's family still shows.
    const attachedIn = (f) => f.root.id === state.attached || f.kids.some((k) => k.id === state.attached);
    const shown = open ? resting : resting.filter(attachedIn);
    if (shown.length) out.push(rows(shown, "resting"));
  }
  return out;
}

// Everything grouped other than by state (Needs you stays above, as
// always), most sessions first:
// - workspace: one group a workspace, in rail order;
// - lead: one group a lead with what it started, then the rest;
// - a label key: one group a value, then those without it.
// A family goes by its lead's workspace and labels.
function groupedList(families, rows, count) {
  const by = state.groupBy;
  const groups = new Map(); // key -> { title, list, rank }
  const add = (key, title, f, rank = 0) => {
    if (!groups.has(key)) groups.set(key, { title, list: [], rank });
    groups.get(key).list.push(f);
  };
  for (const f of families) {
    if (by === GROUP_WORKSPACE) {
      const ws = workspaceById(f.root.workspace_id);
      add(`ws:${f.root.workspace_id}`, ws ? ws.name : "no workspace", f, ws ? state.workspaces.indexOf(ws) : Infinity);
    } else if (by === GROUP_LEAD) {
      if (f.kids.length) add(`lead:${f.root.id}`, f.root.name, f);
      else add("lead:", "no lead", f, 1);
    } else {
      const value = (f.root.labels || {})[by];
      if (value === undefined) add("label:", `no ${by}`, f, 1);
      else add(`label:${value}`, `${by}=${value}`, f);
    }
  }
  const ordered = [...groups.entries()].sort(([ka, a], [kb, b]) =>
    a.rank - b.rank || (by === GROUP_WORKSPACE ? 0 : count(b.list) - count(a.list)) || ka.localeCompare(kb));
  const out = [];
  for (const [key, g] of ordered) out.push(listGroup(g.title, count(g.list)), rows(g.list, key));
  if (!out.length) out.push(el("div", { className: "list-calm", textContent: "Everything else needs you." }));
  return out;
}

// One workspace: its machine (when another's), a way back when agents
// elsewhere need you, its folders, its sessions (families together).
function renderWorkspaceList(workspace) {
  const sessions = state.sessions.filter((s) => s.workspace_id === workspace.id);
  const error = state.wsErrors[workspace.id];
  const elsewhere = attention.entries().filter((e) => e.level < 2 && e.session.workspace_id !== workspace.id).length;
  const filtering = filterTerms(state.filter).length > 0;
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
    !sessions.length
      ? el(
        "div",
        { className: "ws-empty" },
        "No sessions. ",
        el("button", {
          type: "button",
          className: "text-btn",
          textContent: "start one",
          onclick: () => openSessionDialog(workspace.id),
        }),
      )
      : filtering
        ? renderMatches(sessions)
        : el("ul", { className: "sessions" }, families(sessions).flatMap((f) => familyRows(f))),
  ].flat().filter(Boolean);
}

// A press on the sidebar or a pane head holds background repaints (status
// and telemetry pushes, timers, the periodic refresh) until its release:
// a row that moved (to another group, or down the list) between the two
// would take the click with it, or leave it on whatever slid under the
// pointer. A press is never held longer than PRESS_HOLD_MS.
const PRESS_HOLD_MS = 4000;
let pressed = 0;
let pressTimer = 0;
let missedRender = false;
function release() {
  if (!pressed) return;
  pressed = 0;
  clearTimeout(pressTimer);
  if (missedRender) {
    missedRender = false;
    render();
  }
}
document.addEventListener("pointerdown", (event) => {
  if (!event.target.closest || !event.target.closest("#sidebar, .pane-head")) return;
  pressed = Date.now();
  clearTimeout(pressTimer);
  pressTimer = setTimeout(release, PRESS_HOLD_MS);
}, { capture: true });
// After the click the release makes (same task), not before it.
for (const type of ["pointerup", "pointercancel"]) {
  window.addEventListener(type, () => setTimeout(release, 0), { capture: true });
}

// A pointer moving over the lists is aiming at a row: a repaint then
// could slide another row under it (a session changing group). Such
// repaints wait until it has been still for AIM_MS.
const AIM_MS = 700;
let aimedAt = 0;
let aimTimer = 0;
document.addEventListener("pointermove", (event) => {
  if (event.pointerType === "mouse" && event.target.closest && event.target.closest("#side-scroll")) aimedAt = Date.now();
}, { capture: true, passive: true });

// A repaint nobody asked for: held while a press is down or a pointer
// aims at a row.
function backgroundRender() {
  if (pressed) {
    missedRender = true;
    return;
  }
  const aiming = aimedAt + AIM_MS - Date.now();
  if (aiming > 0) {
    clearTimeout(aimTimer);
    aimTimer = setTimeout(backgroundRender, aiming + 20);
    return;
  }
  render();
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
  morph(root, (!state.workspaces.length && !machines.any()
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
  inspector.refresh();
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
    // The state's word, and what it does beside it ("2 agents · 6m").
    const st = agent.state ? statusLook(session) : null;
    const word = st ? st.label : agent.status;
    const doing = st && !st.quiet ? doingOf(session, agent) : null;
    const sub = doing ? [doing.count || (doing.what === "thinking" || doing.what === "turn done" || doing.what === "compacting context" ? "" : doing.what), doing.age].filter(Boolean).join(" \u00b7 ") : "";
    pill.className = `agent-pill ${st ? (st.quiet ? "quiet" : st.dot === "handed" ? "handed" : agent.state) : agent.status}`;
    pill.replaceChildren(word, sub ? el("span", { className: "pill-sub", textContent: sub }) : "");
    pill.title = st ? st.title : agent.status;
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

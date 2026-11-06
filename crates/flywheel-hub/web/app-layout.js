// flywheel hub web client: terminal panes (panes.js, mobile.js), each
// browser's view of a workspace's layout, loading and saving layouts,
// and placing sessions into them.
"use strict";

// --- terminals: panes in tabs and splits ----------------------------
//
// Each workspace has a layout (tabs; each a tree of splits whose leaves
// are panes showing one session), stored on the hub and shared by every
// browser (layout.js, panes.js). Which tab is active, which pane has
// focus, and zoom are this browser's. A phone shows one pane at a time
// (the "solo" pane) and never edits layouts.

const TERM_FONT = '"FW Term", "FW Symbols", ui-monospace, "SF Mono", Menlo, Consolas, monospace';
// Settings' appearance.term_font -> the CSS stack ("FW Term" is JetBrains
// Mono). Every stack ends in the built-in box drawing and symbols (an
// agent's TUI draws them), so no glyph falls to whatever the device has.
const TERM_FONTS = {
  jetbrains: TERM_FONT,
  martian: '"FW Mono", "FW Term", "FW Symbols", ui-monospace, "SF Mono", Menlo, Consolas, monospace',
  system: 'ui-monospace, "SF Mono", Menlo, Consolas, "DejaVu Sans Mono", "FW Term", "FW Symbols", monospace',
};

// The xterm options Settings decides.
function termPrefs() {
  const style = settings.get("terminal.cursor_style", "block");
  const lines = settings.get("terminal.scrollback", 10000);
  return {
    fontFamily: TERM_FONTS[settings.get("appearance.term_font", "jetbrains")] || TERM_FONT,
    fontSize: mobile.fontSize(),
    cursorStyle: ["block", "bar", "underline"].includes(style) ? style : "block",
    cursorBlink: settings.get("terminal.cursor_blink", true) !== false,
    scrollback: Number.isInteger(lines) ? lines : 10000,
  };
}

// Put changed terminal settings on every live pane (a new font is
// loaded before it is measured; the panes refit).
async function applyTermPrefs() {
  const next = termPrefs();
  if (document.fonts && next.fontFamily.startsWith('"FW Mono"')) {
    await document.fonts.load(`${next.fontSize}px "FW Mono"`).catch(() => {});
  }
  for (const pane of livePanes()) {
    const t = pane.term;
    if (!t) continue;
    let refit = false;
    for (const [key, value] of Object.entries(next)) {
      if (t.options[key] === value) continue;
      t.options[key] = value;
      if (key === "fontFamily" || key === "fontSize") refit = true;
    }
    if (refit && pane.refit) pane.refit();
  }
}
const L = window.FwHubLayout;
const phone = () => window.matchMedia(NARROW).matches;
// Wide enough for an agent and its companion shell side by side.
const WIDE = "(min-width: 1101px)";

const focusedPane = () => (phone() ? state.solo : state.panes.get(state.focusId)) || null;
const focusedTerm = () => {
  const pane = focusedPane();
  return pane && pane.sessionId ? pane.term : null;
};
const livePanes = () => (phone() ? (state.solo ? [state.solo] : []) : [...state.panes.values()]);
// The attached session is the focused pane's.
Object.defineProperty(state, "attached", {
  get: () => {
    const pane = focusedPane();
    return pane ? pane.sessionId : null;
  },
});
Object.defineProperty(state, "exited", { get: () => (focusedPane() ? focusedPane().exited : null) });
Object.defineProperty(state, "replaying", { get: () => !!(focusedPane() && focusedPane().replaying) });

const mobile = window.FwHubMobile({
  $, el, state, store, prefs: settings, api, sessionById, agentStatus,
  toast: commands.toast,
  copyText: commands.copyText,
  term: () => focusedTerm(),
  pane: () => focusedPane(),
  panes: () => livePanes(),
  phone: () => phone(),
  sendData: (data) => {
    const pane = focusedPane();
    if (pane) pane.sendInput(JSON.stringify({ type: "input", data }));
  },
  onKey: () => {
    const term = focusedTerm();
    if (term) term.scrollToBottom();
  },
  // The selection popover's "Send to agent" (handoff.js).
  sendSelection: (text) => handoff.sendSelection(state.attached, text),
  // Compose's "Queue" (queue.js).
  canQueue: (session) => promptQueue.canQueue(session),
  queuePrompt: (id, text) => promptQueue.add(id, text),
  // Quick answers' "always": the narrowest rule for the prompt (perms.js).
  allowAlways: (id) => perms.allowFor(id),
});

// A pane resizes its session's PTY (fits it to its box) only while it
// has focus, or while this page is the session's only viewer; else it
// follows the size another viewer set (panes.js).
function claimsSize(pane) {
  if (!pane.sessionId) return false;
  if (pane === focusedPane()) return true;
  const session = sessionById(pane.sessionId);
  return !!(session && session.live && (session.live.clients || 0) <= 1);
}

const panes = window.FwHubPanes({
  el, state, sessionById, TERM_FONT, status: statusLook, goneText, exitHow,
  net,
  // Copy buttons on an agent's replies in its terminal (reply-marks.js).
  replyMarks: window.FwHubReplyMarks({
    el,
    icon,
    sessionById,
    replies,
    paneHidden: (pane) => document.visibilityState !== "visible" || !onStage(pane),
  }),
  // Off screen: the page hidden, or the pane parked (another tab, a zoom).
  paneHidden: (pane) => document.visibilityState !== "visible" || !onStage(pane),
  // A machine's session attaches through this hub's proxy.
  socketPath: (id) => machines.route("GET", `/api/sessions/${id}/ws`),
  // The machine a session is on and how this hub fares with it (null:
  // this hub's own).
  machineLink: (id) => {
    const name = machines.machineOf(id);
    return name ? { name, link: machines.linkOf(name) } : null;
  },
  fontSize: () => mobile.fontSize(),
  termPrefs: () => termPrefs(),
  termTheme: () => ({ theme: TERM_THEMES[resolvedTheme()], minimumContrastRatio: contrastFloor() }),
  transformInput: (data) => mobile.transformInput(data),
  IS_MAC,
  copyText: (text) => commands.copyText(text),
  toast: (...args) => commands.toast(...args),
  fileLinks: (term, sessionOf) => files.linkProvider(term, sessionOf),
  fitTerminal: (pane) => mobile.fitTerminal(pane),
  attachTerminal: (pane) => mobile.attachTerminal(pane.term),
  themeMessage: () => themeMessage(),
  claims: (pane) => claimsSize(pane),
  // Only dividers still on the stage hold (one removed mid-drag, by a
  // rebuild, never fires its release).
  holdFit: () => [...state.dividerHeld].some((d) => d.isConnected),
  onResize: (pane) => {
    if (pane === focusedPane()) updateMeters();
    renderPaneHead(pane);
  },
  onLink: (pane) => {
    if (pane === focusedPane()) setLink(pane.link.kind, pane.link.text);
  },
  onFocusIn: (pane) => {
    if (!phone() && pane.id !== state.focusId) focusPane(pane.id, { fromTerminal: true });
  },
  onAttached: (pane) => {
    // A re-attach (reconnect, hub restart) never takes the keys from an
    // open drawer, panel, or dialog.
    if (pane === focusedPane() && mayTakeFocus()) focusTerminal();
    refresh(false).catch(() => {});
  },
  onExit: (pane) => {
    if (sessionById(pane.sessionId)) refresh().catch(showError);
    if (pane === focusedPane()) renderMain();
  },
  onSessionGone: () => refresh().catch(() => {}),
  relaunch: (id) => relaunch(id),
  closeSession: (id) => closeSession(id),
  refresh: () => refresh(),
});

// --- per-browser view of each workspace's layout ---------------------

// workspace id -> {tab, focus: {tab id: pane id}, zoom: {tab id: pane id}}
const views = (() => {
  const kept = store.get("layout-view", {});
  return kept && typeof kept === "object" ? kept : {};
})();
function viewOf(workspaceId) {
  if (!views[workspaceId] || typeof views[workspaceId] !== "object") views[workspaceId] = {};
  const view = views[workspaceId];
  if (!view.focus || typeof view.focus !== "object") view.focus = {};
  if (!view.zoom || typeof view.zoom !== "object") view.zoom = {};
  return view;
}
const saveViews = () => store.set("layout-view", views);

const layoutOf = (workspaceId) => state.layouts[workspaceId] || null;
const viewLayout = () => layoutOf(state.view);

function activeTab(workspaceId = state.view) {
  const layout = layoutOf(workspaceId);
  if (!layout || !layout.tabs.length) return null;
  const view = viewOf(workspaceId);
  return layout.tabs.find((t) => t.id === view.tab) || layout.tabs[0];
}

// The leaf (data, not pane object) that has focus in the active tab.
function focusedLeaf() {
  const tab = activeTab();
  if (!tab) return null;
  const leaves = L.panes(tab.root);
  return leaves.find((p) => p.id === viewOf(state.view).focus[tab.id]) || leaves[0];
}

const zoomedIn = (tab) => {
  if (!tab) return null;
  const id = viewOf(state.view).zoom[tab.id];
  return id && L.findPane(tab.root, id) ? id : null;
};

// --- layouts: loading and saving -------------------------------------

async function loadLayout(workspaceId) {
  const layout = await api("GET", `/api/workspaces/${workspaceId}/layout`);
  const queue = state.layoutQueue[workspaceId];
  // A write in flight settles it (its answer, or a 409 carrying it).
  if (queue && queue.length) return layoutOf(workspaceId);
  // A GET that started before this page's own save but answered after
  // it is older than what this page already has: keep the newer one.
  const have = state.confirmed[workspaceId];
  if (have && layout && typeof layout.rev === "number" && layout.rev < have.rev) return layoutOf(workspaceId);
  state.confirmed[workspaceId] = layout;
  state.layouts[workspaceId] = L.clone(layout);
  return layout;
}

async function ensureLayout(workspaceId) {
  if (layoutOf(workspaceId)) return layoutOf(workspaceId);
  try {
    return await loadLayout(workspaceId);
  } catch (error) {
    if (!layoutOf(workspaceId)) state.layouts[workspaceId] = { rev: 0, tabs: [] };
    state.confirmed[workspaceId] = state.confirmed[workspaceId] || { rev: 0, tabs: [] };
    throw error;
  }
}

// Change a workspace's layout: `edit(layout)` edits a copy in place (or
// returns false: nothing to do). It shows at once, then is saved; when
// another browser saved first (409), it is replayed onto theirs. Edits
// are saved one at a time, in order.
function editLayout(workspaceId, edit) {
  const shown = L.clone(layoutOf(workspaceId) || { rev: 0, tabs: [] });
  if (edit(shown) === false) return Promise.resolve(false);
  state.layouts[workspaceId] = shown;
  renderStage();
  const queue = state.layoutQueue[workspaceId] || (state.layoutQueue[workspaceId] = []);
  return new Promise((resolve) => {
    queue.push({ edit, resolve });
    if (queue.length === 1) drainLayout(workspaceId);
  });
}

async function drainLayout(workspaceId) {
  const queue = state.layoutQueue[workspaceId];
  while (queue.length) {
    const { edit, resolve } = queue[0];
    let ok = false;
    for (let attempt = 0; attempt < 4; attempt++) {
      const base = L.clone(state.confirmed[workspaceId] || { rev: 0, tabs: [] });
      // Panes the hub holds already stay, though this page may not have
      // heard of their sessions yet (a layout saved elsewhere right after
      // creating them): the hub drops a removed session's panes itself.
      const held = base.tabs.flatMap((t) => L.sessionsOf(t.root));
      if (edit(base) === false) {
        ok = true;
        break;
      }
      // Never send a pane for a session this page has not heard of yet.
      L.prune(base, new Set([...held, ...state.sessions.filter((s) => s.workspace_id === workspaceId).map((s) => s.id)]));
      try {
        state.confirmed[workspaceId] = await putLayout(workspaceId, base);
        ok = true;
        break;
      } catch (error) {
        if (error.layout) {
          state.confirmed[workspaceId] = error.layout;
          continue;
        }
        commands.toast(`Layout not saved: ${error.message}`, { tone: "bad", timeout: 6000 });
        break;
      }
    }
    if (!ok) {
      // Gave up: show what the hub has.
      try {
        state.confirmed[workspaceId] = await api("GET", `/api/workspaces/${workspaceId}/layout`);
      } catch (_) {
        // Unreachable: keep the last confirmed one.
      }
    }
    queue.shift();
    resolve(ok);
  }
  // Settled: show the hub's layout plus nothing pending.
  if (state.confirmed[workspaceId]) {
    state.layouts[workspaceId] = L.clone(state.confirmed[workspaceId]);
    if (workspaceId === state.view) renderStage();
  }
}

async function putLayout(workspaceId, layout) {
  const response = await fetch(machines.route("PUT", `/api/workspaces/${workspaceId}/layout`), {
    method: "PUT",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ rev: layout.rev, tabs: layout.tabs }),
  });
  if (response.status === 401) {
    window.location.replace("/" + window.location.hash);
    throw new Error("signed out");
  }
  const data = await response.json().catch(() => null);
  if (response.ok) return data;
  const error = new Error((data && data.error) || response.statusText);
  if (response.status === 409 && data && data.layout) error.layout = data.layout;
  throw error;
}

// Another browser saved a layout (events socket).
function onLayoutEvent(message) {
  const id = message.workspace_id;
  const confirmed = state.confirmed[id];
  if (confirmed && confirmed.rev >= message.rev) return;
  if (state.layoutQueue[id] && state.layoutQueue[id].length) return;
  if (!confirmed && id !== state.view) return;
  loadLayout(id).then(() => {
    if (id === state.view) renderStage();
  }).catch(() => {});
}

// --- placing sessions ------------------------------------------------

// Show a session: in the pane already showing it (its tab comes to the
// front), else -- `place` -- in the focused pane (`{pane}`: that one), a
// new split beside it (`{split: "right"|"left"|"down"|"up"}`; when
// either half would be under MIN_PANE, `fallback: "swap"` shows it in
// the pane instead, else it gets a new tab; `keepZoom` zooms the new
// pane when the split one was zoomed), or a new tab (`{newTab: true}`).
// Phones show it alone, the layout untouched.
async function openSession(id, place = {}) {
  const session = sessionById(id);
  if (!session) return;
  state.notice = "";
  state.noticeTitle = "";
  delete state.wsErrors[session.workspace_id];
  rememberWorkspace(session.workspace_id);
  document.body.classList.remove("side-open");
  const workspaceId = session.workspace_id;
  try {
    await ensureLayout(workspaceId);
  } catch (_) {
    // Unreachable hub: the layout loads with the next refresh.
  }
  if (!sessionById(id)) return;
  state.view = workspaceId;
  if (phone()) {
    showSolo(id);
    return;
  }
  const layout = layoutOf(workspaceId);
  const tab = activeTab();
  const where = [tab, ...layout.tabs.filter((t) => t !== tab)].filter(Boolean)
    .map((t) => ({ tab: t, leaf: L.paneShowing(t.root, id) }))
    .find((hit) => hit.leaf);
  if (where && !place.split && !place.newTab && !place.pane) {
    selectTab(where.tab.id, where.leaf.id);
    return;
  }
  const view = viewOf(workspaceId);
  if (place.newTab) {
    const fresh = L.tab(L.nextTabName(layout), id);
    if (layout.tabs.length >= L.MAX_TABS) {
      commands.toast(`At most ${L.MAX_TABS} tabs per workspace.`, { timeout: 3000 });
      return;
    }
    view.tab = fresh.id;
    view.focus[fresh.id] = fresh.root.id;
    saveViews();
    await editLayout(workspaceId, (draft) => {
      draft.tabs.push(L.clone(fresh));
    });
    return;
  }
  if (!tab) {
    const fresh = L.tab("main", id);
    view.tab = fresh.id;
    view.focus[fresh.id] = fresh.root.id;
    saveViews();
    await editLayout(workspaceId, (draft) => {
      if (draft.tabs.length) {
        // Another browser laid it out meanwhile: show it in its first tab.
        const first = L.panes(draft.tabs[0].root)[0];
        if (!L.sessionsOf(draft.tabs[0].root).includes(id)) first.session = id;
        return;
      }
      draft.tabs.push(L.clone(fresh));
    });
    return;
  }
  const target = place.pane ? L.findPane(tab.root, place.pane) : focusedLeaf();
  if (!target) return;
  if (place.split && !roomToSplit(tab, target.id, place.split, place.ratio || 0.5)) {
    // Too small to halve (an agent at a few columns is unusable): show
    // it in place (`fallback: "swap"`), else in a new tab.
    if (place.fallback === "swap") {
      place = { pane: target.id };
    } else {
      commands.toast(`No room to split (panes keep ${MIN_PANE.cols}x${MIN_PANE.rows}): opened in a new tab.`, { timeout: 4000 });
      await openSession(id, { newTab: true });
      return;
    }
  }
  if (place.split) {
    if (L.panes(tab.root).length >= L.MAX_PANES && !(where && where.tab === tab)) {
      commands.toast(`At most ${L.MAX_PANES} panes per tab.`, { timeout: 3000 });
      return;
    }
    const freshId = L.newId("p");
    view.focus[tab.id] = freshId;
    // A split made from a zoomed pane can stay zoomed, on the new one.
    if (place.keepZoom && zoomedIn(tab)) view.zoom[tab.id] = freshId;
    else delete view.zoom[tab.id];
    saveViews();
    await editLayout(workspaceId, (draft) => {
      const t = draft.tabs.find((x) => x.id === tab.id);
      if (!t || !L.findPane(t.root, target.id)) return false;
      // Shown elsewhere in this tab: it moves.
      const old = L.paneShowing(t.root, id);
      if (old && old.id === target.id) return false;
      if (old) t.root = L.close(t.root, old.id) || t.root;
      const done = L.split(t.root, target.id, place.split, id, place.ratio || 0.5);
      if (!done) return false;
      done.pane.id = freshId;
      t.root = done.root;
    });
    return;
  }
  view.focus[tab.id] = target.id;
  saveViews();
  await editLayout(workspaceId, (draft) => {
    const t = draft.tabs.find((x) => x.id === tab.id);
    const leaf = t && L.findPane(t.root, target.id);
    if (!leaf || leaf.session === id) return false;
    // Already in this tab in another pane: swap the two.
    const other = L.paneShowing(t.root, id);
    if (other) other.session = leaf.session;
    leaf.session = id;
    if (other && !other.session) t.root = L.close(t.root, other.id) || t.root;
  });
}

// Bring tab `tabId` of the viewed workspace to the front (this browser
// only), focusing `paneId` when given.
function selectTab(tabId, paneId) {
  const view = viewOf(state.view);
  // The stage may still hold another workspace's panes (openSession
  // moves `state.view` first): its tab id says nothing about this one.
  const switching = (activeTab() || {}).id !== tabId || state.stageView !== state.view;
  view.tab = tabId;
  if (paneId) view.focus[tabId] = paneId;
  saveViews();
  if (switching || !state.panes.size) renderStage();
  const leaf = focusedLeaf();
  if (leaf) focusPane(leaf.id);
}

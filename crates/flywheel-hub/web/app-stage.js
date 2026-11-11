// flywheel hub web client: the stage -- the active tab's tree of splits
// and panes, pane focus, zoom, moves, and splitting.
"use strict";

// --- the stage -------------------------------------------------------

// Build the stage for the viewed workspace: the active tab's tree on a
// desktop (panes of other tabs are closed: no sockets, no rendering),
// or the solo pane on a phone.
function renderStage() {
  const host = $("#terminal");
  if (phone()) {
    for (const [id, pane] of state.panes) {
      pane.dispose();
      state.panes.delete(id);
    }
    state.stageKey = "";
    state.stageView = null;
    if (state.solo && state.solo.el.parentNode !== host) host.replaceChildren(state.solo.el);
    if (!state.solo) host.replaceChildren();
    renderTabs();
    renderMain();
    sendView();
    return;
  }
  if (state.solo) {
    state.solo.dispose();
    state.solo = null;
  }
  // Asked before any pane goes: a closed pane takes its focused xterm
  // with it, and the keys typed next must reach the pane that took over.
  const refocus = mayTakeFocus();
  const tab = activeTab();
  const leaves = tab ? L.panes(tab.root) : [];
  for (const [id, pane] of state.panes) {
    if (!leaves.some((leaf) => leaf.id === id && leaf.session)) {
      pane.dispose();
      state.panes.delete(id);
    }
  }
  const leaf = focusedLeaf();
  state.focusId = leaf ? leaf.id : null;
  for (const node of leaves) {
    if (!node.session || !sessionById(node.session)) continue;
    let pane = state.panes.get(node.id);
    if (!pane) {
      pane = panes.create(node.id);
      wirePane(pane);
      state.panes.set(node.id, pane);
    }
    if (pane.sessionId !== node.session) {
      pane.show(node.session);
      pane.shownSeq = state.refreshSeq;
    }
  }
  const zoom = zoomedIn(tab);
  const tree = !tab ? null : zoom ? { type: "pane", id: zoom } : tab.root;
  // Rebuild only when the shape changed (a moved xterm loses focus);
  // a ratio alone is restyled in place.
  const key = JSON.stringify([state.view, tab && tab.id, shape(tree)]);
  state.stageView = state.view;
  if (key !== state.stageKey) {
    state.stageKey = key;
    const park = $("#pane-park");
    for (const pane of state.panes.values()) park.append(pane.el);
    host.replaceChildren(...(tree ? [buildNode(tree, "")] : []));
  } else if (tree) {
    restyle(tree, "");
  }
  host.classList.toggle("multi", !!tree && tree.type === "split");
  for (const pane of state.panes.values()) {
    pane.el.classList.toggle("focused", pane.id === state.focusId);
    renderPaneHead(pane);
  }
  if (refocus) focusTerminal();
  // The header's link pill is the focused pane's, whoever changed focus.
  syncLink();
  renderTabs();
  history.replaceState(null, "", state.attached ? `#session=${state.attached}` : window.location.pathname);
  rememberAttached();
  render();
  sendView();
}

// A pane stands empty until its session is known here (a layout saved
// elsewhere can arrive before the session list): once it is, the shape
// changes and the stage is rebuilt with its terminal.
const shape = (node) => (!node ? null : node.type === "pane"
  ? (node.session === null || !state.panes.has(node.id) ? `${node.id}:empty` : node.id)
  : [node.dir, shape(node.a), shape(node.b)]);

function buildNode(node, path) {
  if (node.type === "pane") {
    const pane = state.panes.get(node.id);
    if (pane) return pane.el;
    return emptyPane(node.id);
  }
  const a = buildNode(node.a, path + "a");
  const b = buildNode(node.b, path + "b");
  const first = el("div", { className: "split-side" }, a);
  const second = el("div", { className: "split-side" }, b);
  const divider = el("div", {
    className: "divider",
    role: "separator",
    tabIndex: 0,
    "aria-orientation": node.dir === "right" ? "vertical" : "horizontal",
    "aria-valuemin": 5,
    "aria-valuemax": 95,
    "aria-label": "Resize panes (arrow keys)",
    title: "Drag to resize (arrow keys when focused; double-click: even)",
    dataset: { path },
  });
  bindDivider(divider, path, node.dir);
  const box = el("div", { className: `split ${node.dir}`, dataset: { path } }, first, divider, second);
  styleSplit(box, node);
  return box;
}

function styleSplit(box, node) {
  const [first, divider, second] = box.children;
  first.style.flex = `${node.ratio} 1 0`;
  second.style.flex = `${1 - node.ratio} 1 0`;
  divider.setAttribute("aria-valuenow", String(Math.round(node.ratio * 100)));
}

function restyle(node, path) {
  if (node.type !== "split") return;
  const box = $(`#terminal .split[data-path="${path}"]`);
  if (box) styleSplit(box, node);
  restyle(node.a, path + "a");
  restyle(node.b, path + "b");
}

// A pane with no session (its session was closed, or a new tab): the
// empty stage stands in for it when it is the tab's only pane.
function emptyPane(id) {
  return el("section", { className: "pane empty-pane", dataset: { pane: id } });
}

// Dividers: drag (pointer) or arrow keys; saved when the drag ends.
function bindDivider(divider, path, dir) {
  const ratioAt = (event) => {
    const box = divider.parentElement.getBoundingClientRect();
    const r = dir === "right" ? (event.clientX - box.left) / box.width : (event.clientY - box.top) / box.height;
    return L.clampRatio(r);
  };
  // Restyled only: the layout changes (and is saved) on commit. While a
  // divider is held the panes keep their terminal size (panes.js refit);
  // letting go fits them once.
  const preview = (ratio) => styleSplit(divider.parentElement, { ratio });
  const hold = () => {
    state.dividerHeld.add(divider);
  };
  const release = () => {
    state.dividerHeld.delete(divider);
    for (const d of state.dividerHeld) if (!d.isConnected) state.dividerHeld.delete(d);
    if (!state.dividerHeld.size) {
      requestAnimationFrame(() => {
        for (const pane of livePanes()) pane.refit();
      });
    }
  };
  const commit = (ratio) => {
    const tab = activeTab();
    if (!tab) return;
    editLayout(state.view, (draft) => {
      const t = draft.tabs.find((x) => x.id === tab.id);
      const node = t && L.splitAt(t.root, path);
      if (!node || node.ratio === ratio) return false;
      node.ratio = ratio;
    });
  };
  divider.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    divider.setPointerCapture(event.pointerId);
    divider.classList.add("dragging");
    document.body.classList.add(dir === "right" ? "resizing-x" : "resizing-y");
    hold();
    let last = null;
    const move = (e) => {
      last = ratioAt(e);
      preview(last);
    };
    const up = () => {
      divider.removeEventListener("pointermove", move);
      divider.classList.remove("dragging");
      document.body.classList.remove("resizing-x", "resizing-y");
      if (last !== null) commit(last);
      release();
    };
    divider.addEventListener("pointermove", move);
    divider.addEventListener("pointerup", up, { once: true });
    divider.addEventListener("pointercancel", up, { once: true });
  });
  divider.addEventListener("dblclick", () => {
    preview(0.5);
    commit(0.5);
  });
  divider.addEventListener("keydown", (event) => {
    const tab = activeTab();
    const node = tab && L.splitAt(tab.root, path);
    if (!node) return;
    const back = dir === "right" ? "ArrowLeft" : "ArrowUp";
    const forth = dir === "right" ? "ArrowRight" : "ArrowDown";
    const step = event.shiftKey ? 0.1 : 0.02;
    let next = null;
    const current = divider.pending ?? node.ratio;
    if (event.key === back) next = current - step;
    else if (event.key === forth) next = current + step;
    else if (event.key === "Home") next = L.MIN_RATIO;
    else if (event.key === "End") next = 1 - L.MIN_RATIO;
    else if (event.key === "Escape" || event.key === "Enter") {
      event.preventDefault();
      focusTerminal();
      return;
    }
    if (next === null) return;
    event.preventDefault();
    event.stopPropagation();
    next = L.clampRatio(next);
    if (divider.pending === undefined) hold();
    divider.pending = next;
    preview(next);
    clearTimeout(divider.saveTimer);
    divider.saveTimer = setTimeout(() => {
      divider.pending = undefined;
      commit(next);
      release();
    }, 400);
  });
}

// Pointer, menus, and drops on a pane.
function wirePane(pane) {
  pane.el.addEventListener("pointerdown", () => {
    if (pane.id !== state.focusId && !phone()) focusPane(pane.id, { fromTerminal: true });
  }, true);
  pane.head.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    paneMenu(pane, event.clientX, event.clientY);
  });
  pane.head.addEventListener("dblclick", (event) => {
    if (!event.target.closest("button")) toggleZoom(pane.id);
  });
  bindDrop(pane.el, () => pane.id);
}

function renderPaneHead(pane) {
  const session = sessionById(pane.sessionId);
  if (session) pane.lastName = session.name;
  if (!session || phone()) {
    pane.head.replaceChildren();
    return;
  }
  const st = statusLook(session);
  const tab = activeTab();
  const zoomed = tab && zoomedIn(tab) === pane.id;
  const size = pane.term ? `${pane.term.cols}×${pane.term.rows}` : "";
  pane.head.replaceChildren(...[
    el("span", { className: `glyph ${glyphFor(session.kind)}` }, icon(glyphFor(session.kind))),
    el("span", { className: `dot ${st.dot}`, title: st.title || st.label }),
    el("span", { className: "pane-name", textContent: session.name, title: session.cwd }),
    el("span", { className: `pane-state ${st.tone}`, textContent: st.label }),
    pane.following
      ? el("span", {
        className: "pane-follow",
        title: "This pane follows the size another viewer set; focus it to fit the terminal to this pane.",
      }, size, el("span", { className: "long", textContent: " · another viewer's size" }))
      : null,
    el(
      "span",
      { className: "pane-tools" },
      splitTool(iconButton("split-right", withKey("Split right: new shell", "pane.split_right"), () => splitWithShell(pane, "right"))),
      splitTool(iconButton("split-down", withKey("Split down: new shell", "pane.split_down"), () => splitWithShell(pane, "down"))),
      sendOutputTool(session),
      iconButton(zoomed ? "unzoom" : "zoom", withKey(zoomed ? "Unzoom" : "Zoom", "pane.zoom"), () => toggleZoom(pane.id)),
      iconButton("dots", "Pane menu", (event) => {
        const box = event.currentTarget.getBoundingClientRect();
        paneMenu(pane, box.left, box.bottom + 2);
      }),
      iconButton("x", withKey("Close pane: the session keeps running", "pane.close"), () => closePane(pane.id)),
    ),
  ].filter(Boolean));
}

// A live shell's "send output" target: its own agent for a companion
// shell, else any agent that can take a paste (null when none can).
function outputTarget(session) {
  const pool = session && !isAgent(session) && session.live ? handoff.targets(session.id) : [];
  if (!pool.length) return null;
  const parent = session.companion_of ? pool.find((s) => s.id === session.companion_of) : null;
  return { label: parent ? `to ${parent.name}` : "to agent", name: parent ? parent.name : "an agent" };
}

function sendOutputTool(session) {
  const target = outputTarget(session);
  if (!target) return null;
  const button = iconButton("send", withKey(`Send the last command's output to ${target.name} (editable first)`, "handoff.output"), () => handoff.sendOutput(session.id));
  button.classList.add("split-tool");
  return button;
}

function splitTool(button) {
  button.classList.add("split-tool");
  return button;
}

// The phone's one pane.
function showSolo(id) {
  if (!state.solo) {
    state.solo = panes.create("solo");
    state.solo.el.classList.add("solo", "focused");
  }
  const host = $("#terminal");
  if (state.solo.el.parentNode !== host) host.replaceChildren(state.solo.el);
  state.solo.show(id);
  state.solo.shownSeq = state.refreshSeq;
  rememberAttached();
  history.replaceState(null, "", `#session=${id}`);
  renderTabs();
  render();
  sendView();
}

// Focus a pane of the active tab (this browser only). It claims the
// PTY size; the one that had focus may go back to following.
function focusPane(paneId, { fromTerminal } = {}) {
  const tab = activeTab();
  if (!tab || !L.findPane(tab.root, paneId)) return;
  const before = focusedPane();
  viewOf(state.view).focus[tab.id] = paneId;
  saveViews();
  state.focusId = paneId;
  const zoom = zoomedIn(tab);
  if (zoom && zoom !== paneId) {
    delete viewOf(state.view).zoom[tab.id];
    renderStage();
  }
  for (const pane of state.panes.values()) pane.el.classList.toggle("focused", pane.id === paneId);
  const pane = state.panes.get(paneId);
  if (pane) {
    if (!fromTerminal) focusTerminal();
    pane.claim();
    setLink(pane.link.kind, pane.link.text);
  } else {
    setLink("idle");
  }
  if (before && before !== pane) before.refit();
  history.replaceState(null, "", state.attached ? `#session=${state.attached}` : window.location.pathname);
  rememberAttached();
  render();
  sendView();
}

// Focus the pane next to the focused one, by where they are on screen.
function focusToward(direction) {
  const current = focusedPane();
  if (!current || phone()) return;
  const from = current.el.getBoundingClientRect();
  const cx = from.left + from.width / 2;
  const cy = from.top + from.height / 2;
  let best = null;
  for (const pane of state.panes.values()) {
    if (pane === current || !onStage(pane)) continue;
    const r = pane.el.getBoundingClientRect();
    const ok = direction === "left" ? r.right <= from.left + 2
      : direction === "right" ? r.left >= from.right - 2
        : direction === "up" ? r.bottom <= from.top + 2
          : r.top >= from.bottom - 2;
    if (!ok) continue;
    const dx = r.left + r.width / 2 - cx;
    const dy = r.top + r.height / 2 - cy;
    // Prefer panes in line with this one.
    const along = direction === "left" || direction === "right" ? Math.abs(dx) : Math.abs(dy);
    const across = direction === "left" || direction === "right" ? Math.abs(dy) : Math.abs(dx);
    const score = along + across * 2;
    if (!best || score < best.score) best = { pane, score };
  }
  if (best) focusPane(best.pane.id);
}

function cyclePane(delta) {
  const tab = activeTab();
  if (!tab || phone()) return;
  const leaves = L.panes(tab.root).filter((p) => state.panes.has(p.id));
  if (leaves.length < 2) return;
  const at = leaves.findIndex((p) => p.id === state.focusId);
  focusPane(leaves[(at + delta + leaves.length) % leaves.length].id);
}

function toggleZoom(paneId = state.focusId) {
  const tab = activeTab();
  if (!tab || !paneId || phone()) return;
  const view = viewOf(state.view);
  if (view.zoom[tab.id] === paneId) delete view.zoom[tab.id];
  else if (L.panes(tab.root).length > 1) view.zoom[tab.id] = paneId;
  else return;
  view.focus[tab.id] = paneId;
  saveViews();
  renderStage();
  const pane = state.panes.get(paneId);
  if (pane) requestAnimationFrame(() => pane.claim());
}

// Resize the focused pane from the keyboard: move its nearest divider.
function nudgePane(direction) {
  const tab = activeTab();
  const paneId = state.focusId;
  if (!tab || !paneId || phone()) return;
  const axis = direction === "left" || direction === "right" ? "right" : "down";
  const step = direction === "left" || direction === "up" ? -0.05 : 0.05;
  editLayout(state.view, (draft) => {
    const t = draft.tabs.find((x) => x.id === tab.id);
    if (!t || !L.resizePane(t.root, paneId, axis, step)) return false;
  });
}

// Take a pane out of the tab; its session keeps running. The last pane
// of a tab closes the tab (an only tab is left empty).
function closePane(paneId = state.focusId) {
  const tab = activeTab();
  if (!tab || !paneId || phone()) return;
  const view = viewOf(state.view);
  const leaves = L.panes(tab.root);
  const at = leaves.findIndex((p) => p.id === paneId);
  if (at < 0) return;
  if (view.focus[tab.id] === paneId) {
    const next = leaves[at + 1] || leaves[at - 1];
    if (next) view.focus[tab.id] = next.id;
  }
  if (view.zoom[tab.id] === paneId) delete view.zoom[tab.id];
  saveViews();
  const layout = viewLayout();
  if (leaves.length === 1 && layout.tabs.length > 1) {
    closeTab(tab.id);
    return;
  }
  editLayout(state.view, (draft) => {
    const t = draft.tabs.find((x) => x.id === tab.id);
    if (!t || !L.findPane(t.root, paneId)) return false;
    t.root = L.close(t.root, paneId) || { type: "pane", id: paneId, session: null };
  });
}

// A new shell beside a pane: an agent's companion (in its directory,
// worktree included) unless that is on screen already, else a shell in
// the session's folder.
async function splitWithShell(pane, side) {
  const session = sessionById(pane && pane.sessionId);
  if (!session) return;
  const tab = activeTab();
  if (tab && L.panes(tab.root).length >= L.MAX_PANES) {
    commands.toast(`At most ${L.MAX_PANES} panes per tab.`, { timeout: 3000 });
    return;
  }
  focusPane(pane.id);
  const agent = isAgent(session);
  const companion = agent ? state.sessions.find((s) => s.companion_of === session.id) : null;
  const companionShown = companion && tab && L.paneShowing(tab.root, companion.id);
  try {
    let created;
    if (agent && !companionShown) {
      created = await api("POST", `/api/sessions/${session.id}/companion`, sizeHint());
    } else {
      const workspace = workspaceById(session.workspace_id);
      const folder = session.worktree ? session.worktree.repo : session.cwd;
      const body = { workspace_id: session.workspace_id, kind: "shell", ...sizeHint() };
      if (workspace && workspace.folders.some((f) => f.path === folder && !f.missing)) body.folder = folder;
      created = await api("POST", "/api/sessions", body);
    }
    await refresh();
    await openSession(created.id || created.session.id, { split: side, pane: pane.id });
  } catch (error) {
    commands.toast(error.message, { tone: "bad", timeout: 6000 });
  }
}

// Alt+` between an agent and its companion shell: side by side on a
// wide screen (palette "Companion shell" turns that off), else in place.
const companionBeside = () => settings.get("keyboard.companion", "split") !== "swap";
function flipTo(id, from) {
  const tab = activeTab();
  if (phone() || !companionBeside() || !window.matchMedia(WIDE).matches || !tab) {
    openSession(id);
    return;
  }
  const shown = L.paneShowing(tab.root, id);
  const zoomed = zoomedIn(tab);
  if (shown) {
    if (zoomed && zoomed !== shown.id) {
      // Zoomed: flip the zoom over to it rather than unzoom the tab.
      const view = viewOf(state.view);
      view.zoom[tab.id] = shown.id;
      view.focus[tab.id] = shown.id;
      saveViews();
      renderStage();
      focusTerminal();
      return;
    }
    focusPane(shown.id);
    return;
  }
  const at = L.paneShowing(tab.root, from);
  const target = sessionById(id);
  // The agent goes back to the left of its shell; with no room for that
  // it takes the pane's place, as on a narrow screen.
  const side = target && target.companion_of ? "right" : "left";
  const place = { split: side, fallback: "swap", keepZoom: true };
  openSession(id, at ? { ...place, pane: at.id } : place);
}

// Smallest pane a split may leave (tmux-like "no space for new pane"):
// an agent's TUI, and the hub's own footer reading, need about this.
const MIN_PANE = { cols: 40, rows: 8 };
const DIVIDER_PX = 7;

// The terminal grid a pane box of `w` x `h` px holds, measured from a
// live pane (cell size, and the title bar and padding around the grid).
function gridOf(w, h) {
  for (const pane of state.panes.values()) {
    const dims = pane.fit && pane.el.isConnected ? pane.fit.proposeDimensions() : null;
    const box = pane.el.getBoundingClientRect();
    if (!dims || !dims.cols || !dims.rows || !box.width || !pane.host.clientWidth) continue;
    const cellW = pane.host.clientWidth / dims.cols;
    const cellH = pane.host.clientHeight / dims.rows;
    return {
      cols: Math.floor((w - (box.width - pane.host.clientWidth)) / cellW),
      rows: Math.floor((h - (box.height - pane.host.clientHeight)) / cellH),
    };
  }
  // Nothing on screen to measure: the terminal font's usual cell.
  const size = mobile.fontSize();
  return { cols: Math.floor((w - 12) / (size * 0.6)), rows: Math.floor((h - 34) / (size * 1.3)) };
}

// Whether splitting pane `paneId` of `tab` toward `side` leaves both
// halves at least MIN_PANE (laid out unzoomed, as other browsers see it).
function roomToSplit(tab, paneId, side, ratio = 0.5) {
  const stage = $("#terminal").getBoundingClientRect();
  if (!stage.width || !stage.height) return true;
  const box = L.boxes(tab.root, stage.width, stage.height, DIVIDER_PX).get(paneId);
  if (!box) return true;
  const share = Math.min(ratio, 1 - ratio);
  const across = side === "right" || side === "left";
  const half = across
    ? { w: (box.w - DIVIDER_PX) * share, h: box.h }
    : { w: box.w, h: (box.h - DIVIDER_PX) * share };
  const grid = gridOf(half.w, half.h);
  return grid.cols >= MIN_PANE.cols && grid.rows >= MIN_PANE.rows;
}

// The focused terminal's size: a reasonable first size for a new one.
function sizeHint() {
  const term = focusedTerm();
  return term ? { cols: term.cols, rows: term.rows } : {};
}

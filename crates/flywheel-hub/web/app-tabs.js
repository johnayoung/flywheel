// flywheel hub web client: tabs, the pane and tab menus, and dragging a
// session from the sidebar onto a pane.
"use strict";

// --- tabs ------------------------------------------------------------

function newTab() {
  const layout = viewLayout();
  if (!state.view || !layout || phone()) return;
  if (layout.tabs.length >= L.MAX_TABS) {
    commands.toast(`At most ${L.MAX_TABS} tabs per workspace.`, { timeout: 3000 });
    return;
  }
  const fresh = L.tab(L.nextTabName(layout), null);
  const view = viewOf(state.view);
  view.tab = fresh.id;
  view.focus[fresh.id] = fresh.root.id;
  saveViews();
  editLayout(state.view, (draft) => {
    if (draft.tabs.length >= L.MAX_TABS) return false;
    draft.tabs.push(L.clone(fresh));
  });
}

function closeTab(tabId) {
  const layout = viewLayout();
  if (!layout) return;
  const at = layout.tabs.findIndex((t) => t.id === tabId);
  if (at < 0) return;
  const view = viewOf(state.view);
  if ((view.tab || layout.tabs[0].id) === tabId) {
    const next = layout.tabs[at + 1] || layout.tabs[at - 1];
    view.tab = next ? next.id : undefined;
  }
  delete view.focus[tabId];
  delete view.zoom[tabId];
  saveViews();
  editLayout(state.view, (draft) => {
    const i = draft.tabs.findIndex((t) => t.id === tabId);
    if (i < 0) return false;
    draft.tabs.splice(i, 1);
  });
}

function cycleTab(delta) {
  const layout = viewLayout();
  const tab = activeTab();
  if (!layout || !tab || layout.tabs.length < 2) return;
  const at = layout.tabs.indexOf(tab);
  selectTab(layout.tabs[(at + delta + layout.tabs.length) % layout.tabs.length].id);
}

function renameTab(tabId, name) {
  const clean = name.trim().slice(0, L.MAX_TAB_NAME);
  const layout = viewLayout();
  if (!clean || !layout) return;
  if (layout.tabs.some((t) => t.id !== tabId && t.name.toLowerCase() === clean.toLowerCase())) {
    commands.toast(`A tab is already named ${clean}.`, { timeout: 3000 });
    return;
  }
  editLayout(state.view, (draft) => {
    const t = draft.tabs.find((x) => x.id === tabId);
    if (!t || t.name === clean) return false;
    t.name = clean;
  });
}

// Move the focused pane's session to a tab of its own.
function paneToTab(pane) {
  const tab = activeTab();
  const layout = viewLayout();
  if (!pane || !pane.sessionId || !tab || !layout) return;
  if (layout.tabs.length >= L.MAX_TABS) {
    commands.toast(`At most ${L.MAX_TABS} tabs per workspace.`, { timeout: 3000 });
    return;
  }
  const sid = pane.sessionId;
  const session = sessionById(sid);
  const fresh = L.tab(L.nextTabName(layout, session ? session.name.slice(0, 24) : "tab"), sid);
  const view = viewOf(state.view);
  view.tab = fresh.id;
  view.focus[fresh.id] = fresh.root.id;
  saveViews();
  editLayout(state.view, (draft) => {
    const t = draft.tabs.find((x) => x.id === tab.id);
    if (t && L.panes(t.root).length > 1) t.root = L.close(t.root, pane.id) || t.root;
    draft.tabs.push(L.clone(fresh));
  });
}

let tabRenaming = null;
function renderTabs() {
  const bar = $("#tabs");
  const layout = viewLayout();
  if (phone()) {
    renderSwitcher(bar, layout);
    return;
  }
  if (!state.view || !layout || !layout.tabs.length) {
    bar.hidden = true;
    bar.replaceChildren();
    return;
  }
  const active = activeTab();
  const workspace = workspaceById(state.view);
  const tabs = layout.tabs.map((t, i) => {
    if (tabRenaming === t.id) {
      const input = el("input", {
        className: "tab-rename",
        value: t.name,
        maxLength: L.MAX_TAB_NAME,
        "aria-label": "Tab name",
        onkeydown: (e) => {
          e.stopPropagation();
          if (e.key === "Enter") {
            tabRenaming = null;
            renameTab(t.id, e.target.value);
            renderTabs();
            focusTerminal();
          } else if (e.key === "Escape") {
            tabRenaming = null;
            renderTabs();
            focusTerminal();
          }
        },
        onblur: (e) => {
          if (tabRenaming !== t.id) return;
          tabRenaming = null;
          renameTab(t.id, e.target.value);
          renderTabs();
        },
      });
      requestAnimationFrame(() => {
        input.focus();
        input.select();
      });
      return el("span", { className: "tab renaming" }, input);
    }
    const count = L.sessionsOf(t.root).length;
    const urgent = L.sessionsOf(t.root).map(sessionById).filter(Boolean)
      .map((s) => agentStatus(s)?.status).find((s) => s === "blocked") ||
      L.sessionsOf(t.root).map(sessionById).filter(Boolean).map((s) => agentStatus(s)?.status).find((s) => s === "done");
    return el(
      "span",
      {
        className: `tab${t === active ? " active" : ""}`,
        role: "tab",
        "aria-selected": String(t === active),
        dataset: { tab: t.id },
      },
      el("button", {
        type: "button",
        className: "tab-pick",
        title: `${t.name}: ${count} pane${count === 1 ? "" : "s"}${i < 9 ? ` (Ctrl B ${i + 1} with the prefix on)` : ""}; double-click to rename`,
        // The first click of a double-click on another tab switches to
        // it and rebuilds the bar, so the second lands on a new button:
        // a click's `detail` (2 on the second) starts the rename either way.
        onclick: (e) => {
          if (e.detail >= 2) {
            tabRenaming = t.id;
            renderTabs();
            return;
          }
          selectTab(t.id);
          focusTerminal();
        },
        oncontextmenu: (e) => {
          e.preventDefault();
          tabMenu(t, e.clientX, e.clientY);
        },
        onauxclick: (e) => {
          if (e.button === 1) closeTab(t.id);
        },
      },
      urgent ? el("span", { className: `tab-dot ${urgent}`, title: `an agent here is ${urgent}` }) : null,
      t.name,
      count > 1 ? el("b", { className: "tab-count", textContent: count }) : null),
      layout.tabs.length > 1 ? iconButton("x", `Close tab ${t.name} (its sessions keep running)`, () => closeTab(t.id)) : null,
    );
  });
  bar.hidden = false;
  bar.replaceChildren(...[
    el("span", { className: "tabs-ws", textContent: workspace ? workspace.name : "" }),
    el("div", { className: "tab-list", role: "tablist", "aria-label": "Tabs" }, tabs),
    layout.tabs.length < L.MAX_TABS ? iconButton("plus", `New tab (${MOD} T)`, newTab) : null,
    zoomedIn(active) ? el("button", { type: "button", className: "zoom-pill", onclick: () => toggleZoom(zoomedIn(active)), title: `Unzoom (${MOD} Z)` }, "zoomed") : null,
  ].filter(Boolean));
}

// Phones: the sessions laid out in the viewed workspace as one row of
// chips -- one per session (a session shown in several tabs is one
// terminal), labelled with the first tab showing it.
function renderSwitcher(bar, layout) {
  const chips = [];
  let activeChip = null;
  if (layout && state.solo && state.solo.sessionId) {
    const multiTab = layout.tabs.length > 1;
    const seen = new Set();
    for (const t of layout.tabs) {
      for (const leaf of L.panes(t.root)) {
        const session = sessionById(leaf.session);
        if (!session || seen.has(session.id)) continue;
        seen.add(session.id);
        const st = statusLook(session);
        const on = leaf.session === state.solo.sessionId;
        // The agent whose companion shell is on screen.
        const shown = sessionById(state.solo.sessionId);
        const parentOf = !on && shown && shown.companion_of === session.id;
        chips.push(el(
          "button",
          {
            type: "button",
            className: `pane-chip${on ? " active" : ""}${parentOf ? " parent" : ""}`,
            "aria-pressed": String(on),
            title: multiTab ? `${session.name} (tab ${t.name})` : session.name,
            onclick: () => {
              const view = viewOf(state.view);
              view.tab = t.id;
              view.focus[t.id] = leaf.id;
              saveViews();
              showSolo(leaf.session);
            },
          },
          el("span", { className: `dot ${st.dot}` }),
          multiTab ? el("small", { textContent: `${t.name}` }) : null,
          session.name,
        ));
        if (on) activeChip = chips[chips.length - 1];
      }
    }
  }
  bar.hidden = chips.length < 2;
  const row = chips.length < 2 ? null : el("div", { className: "pane-switch", role: "toolbar", "aria-label": "Panes" }, chips);
  bar.replaceChildren(...(row ? [row] : []));
  // The chip on screen is in view (the row scrolls sideways).
  if (row && activeChip) {
    requestAnimationFrame(() => {
      const left = activeChip.offsetLeft - row.offsetLeft;
      const right = left + activeChip.offsetWidth;
      if (left < row.scrollLeft) row.scrollLeft = Math.max(0, left - 8);
      else if (right > row.scrollLeft + row.clientWidth) row.scrollLeft = right - row.clientWidth + 8;
    });
  }
}

// --- menus -----------------------------------------------------------

function closeMenu() {
  const menu = $("#ctx-menu");
  if (menu) menu.remove();
}

function showMenu(x, y, items) {
  closeMenu();
  const menu = el("div", { id: "ctx-menu", className: "ctx-menu", role: "menu" }, items.map((item) => item.group
    ? el("div", { className: "ctx-group", role: "presentation", textContent: item.group })
    : el("button", {
      type: "button",
      role: "menuitem",
      className: item.danger ? "danger" : "",
      disabled: !!item.disabled,
      onclick: () => {
        closeMenu();
        item.run();
      },
    }, el("span", { textContent: item.label }), item.hint ? el("kbd", { textContent: item.hint }) : null)));
  document.body.append(menu);
  const box = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(4, Math.min(window.innerWidth - box.width - 4, x))}px`;
  menu.style.top = `${Math.max(4, Math.min(window.innerHeight - box.height - 4, y))}px`;
  const first = menu.querySelector("button:not([disabled])");
  if (first) first.focus();
  menu.addEventListener("keydown", (event) => {
    const buttons = [...menu.querySelectorAll("button:not([disabled])")];
    const at = buttons.indexOf(document.activeElement);
    if (event.key === "Escape") {
      event.preventDefault();
      closeMenu();
      focusTerminal();
    } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const next = buttons[(at + (event.key === "ArrowDown" ? 1 : -1) + buttons.length) % buttons.length];
      if (next) next.focus();
    }
    event.stopPropagation();
  });
}
document.addEventListener("pointerdown", (event) => {
  if (!event.target.closest("#ctx-menu")) closeMenu();
}, true);
window.addEventListener("blur", closeMenu);

function paneMenu(pane, x, y) {
  const session = sessionById(pane.sessionId);
  const tab = activeTab();
  if (!session || !tab) return;
  focusPane(pane.id);
  const shown = new Set(L.sessionsOf(tab.root));
  const others = state.sessions.filter((s) => s.workspace_id === session.workspace_id && !shown.has(s.id));
  const zoomed = zoomedIn(tab) === pane.id;
  const items = [
    { label: "Split right: new shell", hint: `${MOD} \\`, run: () => splitWithShell(pane, "right") },
    { label: "Split down: new shell", hint: `${MOD} -`, run: () => splitWithShell(pane, "down") },
    { label: zoomed ? "Unzoom" : "Zoom", hint: `${MOD} Z`, run: () => toggleZoom(pane.id), disabled: L.panes(tab.root).length < 2 },
    { label: "Move to a new tab", run: () => paneToTab(pane), disabled: L.panes(tab.root).length < 2 },
    { label: "Close pane (keeps running)", hint: `${MOD} X`, run: () => closePane(pane.id) },
    { label: `Close session ${session.name}`, hint: `${MOD} W`, danger: true, run: () => closeSession(session.id) },
  ];
  if (others.length) {
    items.push({ group: "show beside" });
    for (const other of others.slice(0, 8)) {
      items.push({ label: `${other.name} (right)`, run: () => openSession(other.id, { split: "right", pane: pane.id }) });
    }
    items.push({ group: "show here instead" });
    for (const other of others.slice(0, 8)) {
      items.push({ label: other.name, run: () => openSession(other.id, { pane: pane.id }) });
    }
  }
  showMenu(x, y, items);
}

function tabMenu(t, x, y) {
  const layout = viewLayout();
  showMenu(x, y, [
    { label: "Rename", run: () => {
      tabRenaming = t.id;
      renderTabs();
    } },
    { label: "New tab", hint: `${MOD} T`, run: newTab, disabled: layout.tabs.length >= L.MAX_TABS },
    { label: "Close tab (sessions keep running)", danger: true, run: () => closeTab(t.id), disabled: layout.tabs.length < 2 },
  ]);
}

// --- drag a session from the sidebar onto a pane ---------------------

const DRAG_TYPE = "application/x-fw-hub-session";
let dragged = null; // session id being dragged (dataTransfer is unreadable on dragover)

// Which part of a pane the pointer is over: an edge (split that way)
// or the middle (show it here).
function dropZone(box, event) {
  const x = (event.clientX - box.left) / box.width;
  const y = (event.clientY - box.top) / box.height;
  const edges = [["left", x], ["right", 1 - x], ["up", y], ["down", 1 - y]];
  edges.sort((a, b) => a[1] - b[1]);
  return edges[0][1] < 0.28 ? edges[0][0] : "center";
}

function bindDrop(node, paneIdOf) {
  const marker = () => {
    let mark = node.querySelector(":scope > .drop-mark");
    if (!mark) {
      mark = el("div", { className: "drop-mark" });
      node.append(mark);
    }
    return mark;
  };
  const clear = () => {
    const mark = node.querySelector(":scope > .drop-mark");
    if (mark) mark.remove();
  };
  node.addEventListener("dragover", (event) => {
    if (!dragged || phone()) return;
    const session = sessionById(dragged);
    if (!session || session.workspace_id !== state.view) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
    const zone = paneIdOf() && node.classList.contains("pane") && !node.classList.contains("empty-pane")
      ? dropZone(node.getBoundingClientRect(), event)
      : "center";
    marker().dataset.zone = zone;
  });
  node.addEventListener("dragleave", (event) => {
    if (!node.contains(event.relatedTarget)) clear();
  });
  node.addEventListener("drop", (event) => {
    const mark = node.querySelector(":scope > .drop-mark");
    const zone = mark ? mark.dataset.zone : "center";
    clear();
    const id = dragged || event.dataTransfer.getData(DRAG_TYPE);
    dragged = null;
    if (!id || !sessionById(id)) return;
    event.preventDefault();
    const paneId = paneIdOf();
    if (zone === "center") openSession(id, paneId ? { pane: paneId } : {});
    else openSession(id, { split: zone, pane: paneId });
  });
}
bindDrop($("#empty"), () => {
  const leaf = focusedLeaf();
  return leaf ? leaf.id : null;
});
// A session from another workspace cannot join this layout.
document.addEventListener("dragend", () => {
  dragged = null;
  document.querySelectorAll(".drop-mark").forEach((m) => m.remove());
});

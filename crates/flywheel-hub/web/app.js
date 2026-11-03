// flywheel hub web client: workspaces > sessions in the sidebar, each
// workspace's tabs of split panes (one terminal per pane), dialogs, a
// command palette, and keyboard shortcuts.
// Plain JS, no build step. Talks to the hub's REST API, the per-session
// terminal WebSocket, and the hub-wide events WebSocket that pushes agent
// status (protocol: crates/flywheel-hub/src/server.rs).
//
// The client is split by area into classic scripts sharing the page's
// global scope: app-base.js, app-layout.js, app-stage.js, app-tabs.js,
// app-events.js, app-sidebar.js, app-dialogs.js, app-keyboard.js, then
// this file, which boots. index.html loads them in that order, after the
// feature modules (window.FwHub*) they wire. Their top-level const/let are
// shared bindings; their top-level functions are also window properties,
// so a new one must not take a browser global's name (`status` shadows
// the obsolete window.status, harmlessly). A file's top-level
// code runs as it loads and may use only what the files before it (and
// its own earlier lines) declare; function bodies run later and see all.
"use strict";

// --- boot ------------------------------------------------------------

// No #session (a fresh tab, a restart): bring back the last workspace
// this browser had open -- else the first with a saved layout -- at
// its active tab's focused pane.
async function restoreWorkspace() {
  const remembered = workspaceById(state.currentWorkspace);
  const order = [remembered, ...state.workspaces.filter((w) => w !== remembered)].filter(Boolean);
  for (const workspace of order) {
    let layout;
    try {
      layout = await ensureLayout(workspace.id);
    } catch (_) {
      continue;
    }
    if (state.attached || window.location.hash) return;
    if (!layout || !layout.tabs.length) continue;
    const view = viewOf(workspace.id);
    const tab = layout.tabs.find((t) => t.id === view.tab) || layout.tabs[0];
    const leaves = L.panes(tab.root).filter((leaf) => leaf.session && sessionById(leaf.session));
    const leaf = leaves.find((p) => p.id === view.focus[tab.id]) || leaves[0];
    const session = leaf ? leaf.session
      : layout.tabs.flatMap((t) => L.sessionsOf(t.root)).find((id) => sessionById(id));
    if (session) {
      await openSession(session);
      return;
    }
  }
}

async function boot() {
  // Measure terminal cells with the real font, not a fallback.
  const fonts = document.fonts
    ? Promise.all([
      document.fonts.load('13px "FW Term"'),
      document.fonts.load('600 13px "FW Term"'),
    ])
    : Promise.resolve();
  await Promise.race([fonts, new Promise((r) => setTimeout(r, 1500))]).catch(() => {});
  applyTheme(theme);
  loadKinds();
  // Static chord labels follow the platform's hub modifier.
  document.querySelectorAll("kbd").forEach((k) => {
    if (k.textContent === "Alt") k.textContent = MOD;
  });
  document.querySelectorAll("[title]").forEach((node) => {
    node.title = node.title.replace(/\bAlt\b/g, MOD);
  });
  // Other machines' lists first, so a #session= of theirs re-attaches.
  await machines.start();
  try {
    await refresh();
  } catch (error) {
    showError(error);
  }
  // Re-attach the session named in the URL (survives reloads).
  const match = /^#session=([0-9a-f]+)$/.exec(window.location.hash);
  if (match) {
    if (sessionById(match[1])) openSession(match[1]);
    else detach(STALE_LINK, "Session removed.");
  } else if (!phone()) {
    // A phone lands on its start screen (Needs you): opening a layout
    // pane here would mark an unseen finished turn seen unasked.
    await restoreWorkspace().catch(() => {});
  }
  // Follow an edited or navigated #session= (replaceState fires no
  // hashchange, so this sees only the user's own changes).
  window.addEventListener("hashchange", () => {
    const target = /^#session=([0-9a-f]+)$/.exec(window.location.hash);
    if (!target) {
      if (state.attached) detach();
      return;
    }
    if (target[1] === state.attached) return;
    if (sessionById(target[1])) openSession(target[1]);
    else if (state.attached) history.replaceState(null, "", `#session=${state.attached}`);
    else detach(STALE_LINK, "Session removed.");
  });
  startPolling();
  connectEvents();
}

boot();

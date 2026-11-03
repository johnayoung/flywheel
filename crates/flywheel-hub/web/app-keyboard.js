// flywheel hub web client: the command palette, keyboard shortcuts,
// and the header's data-action buttons.
"use strict";

// --- command palette -------------------------------------------------

const palette = {
  items: [],
  shown: [],
  index: 0,
};

function paletteCommands() {
  const attached = sessionById(state.attached);
  // Hints name the keys in force (Settings: keyboard).
  const k = (id) => settings.keyOf(id);
  const prefixLabel = k("prefix.arm") || "(unbound)";
  const jump = (settings.keymap()["session.jump"] || [])[0] || "";
  const items = [];
  const numbering = new Map(attachable().map((s, i) => [s.id, i]));
  for (const session of orderedSessions()) {
    const workspace = workspaceById(session.workspace_id);
    const n = numbering.get(session.id);
    items.push({
      group: "Sessions",
      glyph: glyphFor(session.kind),
      label: session.name,
      hint: `${session.machine ? `${session.machine} · ` : ""}${workspace ? workspace.name : ""} · ${statusLook(session).label}${n < 9 && jump ? ` · ${settings.label(jump.replace("1..9", String(n + 1)))}` : ""}${session.live ? "" : " · no terminal"}`,
      run: () => openSession(session.id),
    });
  }
  const cmd = (label, hint, run, glyph = "chev", extra = {}) => items.push({ group: "Commands", glyph, label, hint, run, ...extra });
  cmd("New session", k("session.new"), () => openSessionDialog(), "plus");
  for (const workspace of state.workspaces) {
    cmd(`New session in ${workspace.name}`, workspace.machine ? `on ${workspace.machine}` : "", () => openSessionDialog(workspace.id), "plus");
  }
  cmd("New workspace", k("workspace.new"), () => openWorkspaceDialog(), "plus");
  cmd("Machines", "other machines' hubs: add, test, remove", () => machines.openDialog(), "chev");
  for (const name of machines.names()) {
    if (machines.linkOf(name) !== "online") cmd(`Reconnect to ${name}`, machines.linkOf(name), () => machines.reconnect(name), "redo");
  }
  cmd("Import VS Code workspace", ".code-workspace files", () => {
    openWorkspaceDialog();
    importer.open();
  }, "import");
  if (attached) {
    cmd(`Rename ${attached.name}`, k("session.rename"), () => startRename(attached.id), "pen");
    if (attached.state === "running" && attached.live) {
      cmd(`Stop ${attached.name}`, "keep scrollback", () => stopSession(attached.id), "stop");
    } else {
      cmd(`Relaunch ${attached.name}`, "same session, fresh process", () => relaunch(attached.id), "redo");
    }
    cmd(`Close ${attached.name}`, k("session.close"), () => closeSession(attached.id), "x");
    cmd("Detach", "close every pane here; the sessions keep running", () => detach(), "chev");
  }
  if (state.view && !phone()) {
    const tab = activeTab();
    const pane = focusedPane();
    if (pane && pane.sessionId) {
      cmd("Split right: new shell", k("pane.split_right"), () => splitWithShell(pane, "right"), "split-right");
      cmd("Split down: new shell", k("pane.split_down"), () => splitWithShell(pane, "down"), "split-down");
      if (tab && L.panes(tab.root).length > 1) {
        cmd(zoomedIn(tab) ? "Unzoom pane" : "Zoom pane", k("pane.zoom"), () => toggleZoom(), "zoom");
        cmd("Move pane to a new tab", "", () => paneToTab(pane), "chev");
      }
      cmd("Close pane", `${k("pane.close")} · the session keeps running`, () => closePane(), "x");
    }
    cmd("New tab", k("tab.new"), () => newTab(), "plus");
    const layout = viewLayout();
    if (layout) {
      layout.tabs.forEach((t, i) => {
        if (t !== tab) cmd(`Tab: ${t.name}`, `${L.panes(t.root).length} panes${prefix.on && i < 9 ? ` · ${prefixLabel} ${i + 1}` : ""}`, () => selectTab(t.id), "chev");
      });
      if (tab) {
        cmd(`Rename tab ${tab.name}`, prefix.on ? `${prefixLabel} ${settings.keyOf("prefix.rename_tab")}` : "double-click the tab", () => {
          tabRenaming = tab.id;
          renderTabs();
        }, "pen");
      }
    }
  }
  cmd(`Pane prefix ${prefixLabel}: turn ${prefix.on ? "off" : "on"}`, prefix.on ? `now on (${prefixLabel} twice sends it)` : `tmux-style: ${prefixLabel} then a key (Settings: keyboard)`, () => {
    const on = !prefix.on;
    setPrefixOn(on);
    settings.set("keyboard.pane_prefix", on);
    commands.toast(on ? `Pane prefix on: ${prefixLabel}, then a key (${prefixLabel} twice sends it).` : `Pane prefix off: ${prefixLabel} goes to the terminal.`, { timeout: 3500 });
  }, "gear");
  const beside = companionBeside();
  cmd(`Companion shell: ${beside ? "in place" : "beside the agent"}`, `${settings.keyOf("companion.flip")} ${beside ? "now opens it beside (wide screens)" : "now flips in place"}`, () => {
    settings.set("keyboard.companion", beside ? "swap" : "split");
  }, "gear");
  launcher.paletteItems(cmd);
  search.paletteItems(cmd);
  commands.paletteItems(cmd);
  flywheel.paletteItems(cmd);
  fanout.paletteItems(cmd);
  review.paletteItems(cmd);
  mobile.paletteItems(cmd);
  notify.paletteItems(cmd);
  for (const workspace of state.workspaces) {
    cmd(`Edit workspace ${workspace.name}`, "rename, folders", () => openWorkspaceDialog(workspace), "gear");
  }
  for (const t of THEMES) {
    cmd(`Theme: ${t}`, t === theme ? "current" : "", () => applyTheme(t), "gear");
  }
  cmd("Keyboard shortcuts", settings.keyOf("keys.show"), () => settings.showKeys(), "chev");
  settings.paletteItems(cmd);
  cmd("Sign out", "", logout, "x");
  return items;
}

// Subsequence match; returns [score, matched indices] or null.
function fuzzy(query, text) {
  if (!query) return [0, []];
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  const hits = [];
  let score = 0;
  let from = 0;
  let last = -2;
  for (const ch of q) {
    if (ch === " ") continue;
    const at = t.indexOf(ch, from);
    if (at < 0) return null;
    hits.push(at);
    score += at === last + 1 ? 6 : 1;
    if (at === 0 || /[\s/._-]/.test(t[at - 1])) score += 4;
    last = at;
    from = at + 1;
  }
  return [score - t.length * 0.02, hits];
}

function highlight(text, hits) {
  if (!hits.length) return [text];
  const set = new Set(hits);
  const out = [];
  let run = "";
  let marked = false;
  for (let i = 0; i < text.length; i++) {
    const m = set.has(i);
    if (m !== marked && run) {
      out.push(marked ? el("mark", { textContent: run }) : run);
      run = "";
    }
    marked = m;
    run += text[i];
  }
  if (run) out.push(marked ? el("mark", { textContent: run }) : run);
  return out;
}

function renderPalette() {
  const query = $("#palette-input").value.trim();
  const scored = [];
  for (const item of palette.items) {
    const match = fuzzy(query, item.label);
    const hintMatch = match ? null : fuzzy(query, item.hint);
    if (match) scored.push({ item, score: match[0] + 1, hits: match[1] });
    else if (hintMatch) scored.push({ item, score: hintMatch[0] - 5, hits: [] });
  }
  if (query) scored.sort((a, b) => b.score - a.score);
  palette.shown = scored;
  palette.index = Math.min(palette.index, Math.max(0, scored.length - 1));
  const list = $("#palette-list");
  const nodes = [];
  let group = null;
  scored.forEach(({ item, hits }, i) => {
    if (!query && item.group !== group) {
      group = item.group;
      nodes.push(el("li", { className: "palette-group", role: "presentation", textContent: group }));
    }
    nodes.push(
      el(
        "li",
        {
          className: `palette-item${item.warn ? " warn" : ""}`,
          role: "option",
          id: `palette-${i}`,
          "aria-selected": String(i === palette.index),
          onmousemove: () => {
            if (palette.index !== i) {
              palette.index = i;
              markPalette();
            }
          },
          onclick: () => runPalette(i),
        },
        el("span", { className: `glyph ${item.glyph}` }, icon(item.glyph)),
        el("span", { className: "label" }, highlight(item.label, hits)),
        el("span", { className: "hint", textContent: item.hint }),
      ),
    );
  });
  if (!scored.length) nodes.push(el("li", { className: "palette-none", textContent: "No match." }));
  list.replaceChildren(...nodes);
  markPalette();
}

function markPalette() {
  $("#palette-list").querySelectorAll(".palette-item").forEach((node) => {
    const selected = node.id === `palette-${palette.index}`;
    node.setAttribute("aria-selected", String(selected));
    if (selected) node.scrollIntoView({ block: "nearest" });
  });
  $("#palette-input").setAttribute("aria-activedescendant", `palette-${palette.index}`);
}

const paletteOpen = () => !$("#palette").hidden;

// `query`: a filter to start from. `items`: a list of its own (the
// quick-launch list) instead of every command, with `placeholder`.
function openPalette(query, items, placeholder) {
  explain.close();
  palette.items = items || paletteCommands();
  palette.index = 0;
  $("#palette-input").placeholder = placeholder || "Jump to a session or run a command";
  $("#palette-input").value = typeof query === "string" ? query : "";
  $("#palette").hidden = false;
  renderPalette();
  $("#palette-input").focus();
}

function closePalette() {
  $("#palette").hidden = true;
  focusTerminal();
}

function runPalette(i) {
  const entry = palette.shown[i];
  closePalette();
  if (entry) entry.item.run();
}

$("#palette-input").addEventListener("input", () => {
  palette.index = 0;
  renderPalette();
});
$("#palette-input").addEventListener("keydown", (event) => {
  const n = palette.shown.length;
  if (event.key === "ArrowDown" || (event.ctrlKey && event.key === "n")) {
    event.preventDefault();
    palette.index = n ? (palette.index + 1) % n : 0;
    markPalette();
  } else if (event.key === "ArrowUp" || (event.ctrlKey && event.key === "p")) {
    event.preventDefault();
    palette.index = n ? (palette.index - 1 + n) % n : 0;
    markPalette();
  } else if (event.key === "Enter") {
    event.preventDefault();
    runPalette(palette.index);
  } else if (event.key === "Escape") {
    event.preventDefault();
    closePalette();
  }
});
$("#palette").addEventListener("mousedown", (event) => {
  if (event.target === $("#palette")) closePalette();
});

// --- keyboard --------------------------------------------------------

function jumpBy(delta) {
  const live = attachable();
  if (!live.length) return;
  const at = live.findIndex((s) => s.id === state.attached);
  const next = at < 0 ? (delta > 0 ? 0 : live.length - 1) : (at + delta + live.length) % live.length;
  openSession(live[next].id);
}

const ARROWS = { ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down" };

// The tmux-style prefix (off unless turned on: Settings, or palette "Pane
// prefix"): its key (default Ctrl+B), then one key. The prefix key twice
// sends it to the terminal.
const prefix = { on: settings.get("keyboard.pane_prefix", false) === true, armed: false, timer: 0 };
function armPrefix(armed) {
  prefix.armed = armed;
  clearTimeout(prefix.timer);
  if (armed) prefix.timer = setTimeout(() => armPrefix(false), 2500);
  $("#prefix-pill").hidden = !armed;
}
function setPrefixOn(on) {
  prefix.on = on;
  armPrefix(false);
}

// The control character a Ctrl+letter key sends (Ctrl+B -> ^B), or "".
function controlChar(event) {
  const m = /^Key([A-Z])$/.exec(event.code);
  return event.ctrlKey && m ? String.fromCharCode(m[1].charCodeAt(0) - 64) : "";
}

const isPrefixKey = (event) => {
  const hit = settings.match(event);
  return !!hit && hit.id === "prefix.arm";
};

// What each prefix action does (index: the nth of a 1..9 range).
const PREFIX_ACTIONS = {
  "prefix.split_right": () => splitWithShell(focusedPane(), "right"),
  "prefix.split_down": () => splitWithShell(focusedPane(), "down"),
  "prefix.focus_left": () => focusToward("left"),
  "prefix.focus_down": () => focusToward("down"),
  "prefix.focus_up": () => focusToward("up"),
  "prefix.focus_right": () => focusToward("right"),
  "prefix.resize_left": () => nudgePane("left"),
  "prefix.resize_down": () => nudgePane("down"),
  "prefix.resize_up": () => nudgePane("up"),
  "prefix.resize_right": () => nudgePane("right"),
  "prefix.zoom": () => toggleZoom(),
  "prefix.close_pane": () => closePane(),
  "prefix.new_tab": () => newTab(),
  "prefix.next_tab": () => cycleTab(1),
  "prefix.prev_tab": () => cycleTab(-1),
  "prefix.next_pane": () => cyclePane(1),
  "prefix.tab": (n) => {
    const layout = viewLayout();
    const t = layout && layout.tabs[n];
    if (t) selectTab(t.id);
  },
  "prefix.rename_tab": () => {
    const t = activeTab();
    if (t) {
      tabRenaming = t.id;
      renderTabs();
    }
  },
  "prefix.close_tab": () => {
    const t = activeTab();
    if (t) closeTab(t.id);
  },
  "prefix.help": () => settings.showKeys(),
};

// Returns true when the key was the prefix's (and is consumed).
function prefixKey(event) {
  if (!prefix.on || phone()) return false;
  if (!prefix.armed) {
    if (!isPrefixKey(event)) return false;
    event.preventDefault();
    event.stopPropagation();
    armPrefix(true);
    return true;
  }
  if (["Shift", "Control", "Alt", "Meta", "CapsLock"].includes(event.key)) return false;
  event.preventDefault();
  event.stopPropagation();
  armPrefix(false);
  if (isPrefixKey(event)) {
    const pane = focusedPane();
    const literal = controlChar(event);
    if (pane && literal) pane.send(JSON.stringify({ type: "input", data: literal }));
    return true;
  }
  const hit = settings.matchPrefix(event);
  if (hit && PREFIX_ACTIONS[hit.id]) PREFIX_ACTIONS[hit.id](hit.index);
  return true;
}

// What each global action does; returning false leaves the key to the
// terminal (the action does not apply here and now).
const GLOBAL_ACTIONS = {
  "session.new": () => openSessionDialog(),
  "workspace.new": () => openWorkspaceDialog(),
  "session.jump": (n) => {
    const target = attachable()[n];
    if (!target) return false;
    openSession(target.id);
    return true;
  },
  "session.next": () => jumpBy(1),
  "session.prev": () => jumpBy(-1),
  "session.rename": () => (state.attached ? startRename(state.attached) : false),
  "session.close": () => (state.attached ? closeSession(state.attached) : false),
  "commands.toggle": () => {
    review.close();
    commands.toggle();
  },
  "commands.copy_last": () => commands.copyLast(),
  "companion.flip": () => (state.attached ? commands.flipCompanion() : false),
  "flywheel.toggle": () => flywheel.togglePill(),
  "review.toggle": () => review.togglePill(),
  "fanout.open": () => {
    document.body.classList.remove("side-open");
    fanout.open();
  },
  "fanout.new": () => {
    document.body.classList.remove("side-open");
    fanout.openLaunch();
  },
  "pane.split_right": () => splitWithShell(focusedPane(), "right"),
  "pane.split_down": () => splitWithShell(focusedPane(), "down"),
  "pane.focus_left": () => focusToward("left"),
  "pane.focus_down": () => focusToward("down"),
  "pane.focus_up": () => focusToward("up"),
  "pane.focus_right": () => focusToward("right"),
  "pane.resize_left": () => nudgePane("left"),
  "pane.resize_down": () => nudgePane("down"),
  "pane.resize_up": () => nudgePane("up"),
  "pane.resize_right": () => nudgePane("right"),
  "pane.zoom": () => toggleZoom(),
  "pane.close": () => (state.focusId && !phone() ? closePane() : false),
  "tab.new": () => (state.view && !phone() ? newTab() : false),
  "tab.prev": () => cycleTab(-1),
  "tab.next": () => cycleTab(1),
  "settings.open": () => settings.open(),
  "keys.show": () => settings.showKeys(),
};

// Capture phase: runs before xterm sees the key, so hub shortcuts never
// reach the shell. Every binding comes from the keymap (Settings).
window.addEventListener(
  "keydown",
  (event) => {
    if (settings.isCapturing()) return;
    const hit = settings.match(event);
    if (hit && hit.id === "palette.open") {
      event.preventDefault();
      event.stopPropagation();
      if (paletteOpen()) {
        // Again inside the palette: the key itself, to the terminal.
        closePalette();
        const literal = controlChar(event);
        if (focusedPane() && literal) focusedPane().send(JSON.stringify({ type: "input", data: literal }));
      } else if (!document.querySelector("dialog[open]")) {
        search.close(false);
        openPalette();
      }
      return;
    }
    if (hit && hit.id === "search.open") {
      event.preventDefault();
      event.stopPropagation();
      if (paletteOpen()) closePalette();
      if (!document.querySelector("dialog[open]")) search.toggle();
      return;
    }
    if (paletteOpen() || search.isOpen() || document.querySelector("dialog[open]") || $("#ctx-menu")) return;
    if (prefixKey(event)) return;
    if (!hit || hit.id === "prefix.arm" || !GLOBAL_ACTIONS[hit.id]) return;
    if (GLOBAL_ACTIONS[hit.id](hit.index) === false) return;
    event.preventDefault();
    event.stopPropagation();
  },
  true,
);

// --- chrome actions --------------------------------------------------

async function logout() {
  pwa.forget();
  mobile.forget();
  await fetch("/logout", { method: "POST", credentials: "same-origin" });
  window.location.replace("/");
}

const actions = {
  "new-session": () => openSessionDialog(),
  "new-workspace": () => openWorkspaceDialog(),
  palette: () => openPalette(),
  "from-template": () => {
    document.body.classList.remove("side-open");
    openPalette("", launcher.templateItems(), "Launch from a template");
  },
  fanouts: () => {
    document.body.classList.remove("side-open");
    fanout.open();
  },
  "cycle-theme": () => applyTheme(THEMES[(THEMES.indexOf(theme) + 1) % THEMES.length]),
  logout,
  "toggle-sidebar": () => document.body.classList.toggle("side-open"),
  "toggle-commands": () => {
    review.close();
    commands.toggle();
  },
  "flip-companion": () => commands.flipCompanion(),
  "toggle-flywheel": () => flywheel.togglePill(),
  "toggle-review": () => review.togglePill(),
  search: () => search.open(),
  settings: () => {
    document.body.classList.remove("side-open");
    settings.open();
  },
  machines: () => {
    document.body.classList.remove("side-open");
    machines.openDialog();
  },
};
document.addEventListener("click", (event) => {
  const target = event.target.closest("[data-action]");
  if (target && actions[target.dataset.action]) actions[target.dataset.action]();
});

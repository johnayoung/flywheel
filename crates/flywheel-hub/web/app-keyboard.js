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
  const items = [];
  const numbering = new Map(attachable().map((s, i) => [s.id, i]));
  for (const session of orderedSessions()) {
    const workspace = workspaceById(session.workspace_id);
    const n = numbering.get(session.id);
    items.push({
      group: "Sessions",
      glyph: glyphFor(session.kind),
      label: session.name,
      hint: `${session.machine ? `${session.machine} · ` : ""}${workspace ? workspace.name : ""} · ${status(session).label}${n < 9 ? ` · ${MOD} ${n + 1}` : ""}${session.live ? "" : " · no terminal"}`,
      run: () => openSession(session.id),
    });
  }
  const cmd = (label, hint, run, glyph = "chev", extra = {}) => items.push({ group: "Commands", glyph, label, hint, run, ...extra });
  cmd("New session", `${MOD} N`, () => openSessionDialog(), "plus");
  for (const workspace of state.workspaces) {
    cmd(`New session in ${workspace.name}`, workspace.machine ? `on ${workspace.machine}` : "", () => openSessionDialog(workspace.id), "plus");
  }
  cmd("New workspace", `${MOD} Shift N`, () => openWorkspaceDialog(), "plus");
  cmd("Machines", "other machines' hubs: add, test, remove", () => machines.openDialog(), "chev");
  for (const name of machines.names()) {
    if (machines.linkOf(name) !== "online") cmd(`Reconnect to ${name}`, machines.linkOf(name), () => machines.reconnect(name), "redo");
  }
  cmd("Import VS Code workspace", ".code-workspace files", () => {
    openWorkspaceDialog();
    importer.open();
  }, "import");
  if (attached) {
    cmd(`Rename ${attached.name}`, `${MOD} R`, () => startRename(attached.id), "pen");
    if (attached.state === "running" && attached.live) {
      cmd(`Stop ${attached.name}`, "keep scrollback", () => stopSession(attached.id), "stop");
    } else {
      cmd(`Relaunch ${attached.name}`, "same session, fresh process", () => relaunch(attached.id), "redo");
    }
    cmd(`Close ${attached.name}`, `${MOD} W`, () => closeSession(attached.id), "x");
    cmd("Detach", "close every pane here; the sessions keep running", () => detach(), "chev");
  }
  if (state.view && !phone()) {
    const tab = activeTab();
    const pane = focusedPane();
    if (pane && pane.sessionId) {
      cmd("Split right: new shell", `${MOD} \\`, () => splitWithShell(pane, "right"), "split-right");
      cmd("Split down: new shell", `${MOD} -`, () => splitWithShell(pane, "down"), "split-down");
      if (tab && L.panes(tab.root).length > 1) {
        cmd(zoomedIn(tab) ? "Unzoom pane" : "Zoom pane", `${MOD} Z`, () => toggleZoom(), "zoom");
        cmd("Move pane to a new tab", "", () => paneToTab(pane), "chev");
      }
      cmd("Close pane", `${MOD} X · the session keeps running`, () => closePane(), "x");
    }
    cmd("New tab", `${MOD} T`, () => newTab(), "plus");
    const layout = viewLayout();
    if (layout) {
      layout.tabs.forEach((t, i) => {
        if (t !== tab) cmd(`Tab: ${t.name}`, `${L.panes(t.root).length} panes${prefix.on && i < 9 ? ` · Ctrl B ${i + 1}` : ""}`, () => selectTab(t.id), "chev");
      });
      if (tab) {
        cmd(`Rename tab ${tab.name}`, prefix.on ? "Ctrl B ," : "double-click the tab", () => {
          tabRenaming = tab.id;
          renderTabs();
        }, "pen");
      }
    }
  }
  cmd(`Pane prefix Ctrl B: turn ${prefix.on ? "off" : "on"}`, prefix.on ? "now on (Ctrl B twice sends ^B)" : "tmux-style: Ctrl B then % \" arrows z x c n p", () => {
    prefix.on = !prefix.on;
    store.set("prefix", prefix.on);
    armPrefix(false);
    commands.toast(prefix.on ? "Pane prefix on: Ctrl B, then a key (Ctrl B twice sends ^B)." : "Pane prefix off: Ctrl B goes to the terminal.", { timeout: 3500 });
  }, "gear");
  const beside = companionBeside();
  cmd(`Companion shell: ${beside ? "in place" : "beside the agent"}`, `${MOD} \` ${beside ? "now opens it beside (wide screens)" : "now flips in place"}`, () => {
    store.set("companion", beside ? "swap" : "split");
  }, "gear");
  launcher.paletteItems(cmd);
  search.paletteItems(cmd);
  commands.paletteItems(cmd);
  flywheel.paletteItems(cmd);
  review.paletteItems(cmd);
  mobile.paletteItems(cmd);
  notify.paletteItems(cmd);
  for (const workspace of state.workspaces) {
    cmd(`Edit workspace ${workspace.name}`, "rename, folders", () => openWorkspaceDialog(workspace), "gear");
  }
  for (const t of THEMES) {
    cmd(`Theme: ${t}`, t === theme ? "current" : "", () => applyTheme(t), "gear");
  }
  cmd("Keyboard shortcuts", "", () => $("#dlg-keys").showModal(), "chev");
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

// The hub chord: Alt (no Ctrl/Meta) elsewhere, Ctrl+Option on macOS.
function hubChord(event) {
  if (event.metaKey) return false;
  return IS_MAC ? event.ctrlKey && event.altKey : event.altKey && !event.ctrlKey;
}

// Whether the key reports its plain character (Alt+7 -> "7"), as
// opposed to a layout character or a dead key.
const typesItself = (event, ch) => event.key.length === 1 && event.key.toUpperCase() === ch;

const ARROWS = { ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down" };
const VIM = { KeyH: "left", KeyJ: "down", KeyK: "up", KeyL: "right" };

// The tmux-style prefix (off unless turned on: palette "Pane prefix"):
// Ctrl+B, then one key. Ctrl+B twice sends a literal ^B.
const prefix = { on: store.get("prefix", false) === true, armed: false, timer: 0 };
function armPrefix(armed) {
  prefix.armed = armed;
  clearTimeout(prefix.timer);
  if (armed) prefix.timer = setTimeout(() => armPrefix(false), 2500);
  $("#prefix-pill").hidden = !armed;
}

// Returns true when the key was the prefix's (and is consumed).
function prefixKey(event) {
  if (!prefix.on || phone()) return false;
  const ctrlB = event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey && event.code === "KeyB";
  if (!prefix.armed) {
    if (!ctrlB) return false;
    event.preventDefault();
    event.stopPropagation();
    armPrefix(true);
    return true;
  }
  if (["Shift", "Control", "Alt", "Meta", "CapsLock"].includes(event.key)) return false;
  event.preventDefault();
  event.stopPropagation();
  armPrefix(false);
  const key = event.key;
  const pane = focusedPane();
  if (ctrlB) {
    if (pane) pane.send(JSON.stringify({ type: "input", data: "\x02" }));
  } else if (key === "%") {
    splitWithShell(pane, "right");
  } else if (key === '"') {
    splitWithShell(pane, "down");
  } else if (ARROWS[event.code] && event.ctrlKey) {
    nudgePane(ARROWS[event.code]);
  } else if (ARROWS[event.code]) {
    focusToward(ARROWS[event.code]);
  } else if (key === "z") {
    toggleZoom();
  } else if (key === "x") {
    closePane();
  } else if (key === "c") {
    newTab();
  } else if (key === "n") {
    cycleTab(1);
  } else if (key === "p") {
    cycleTab(-1);
  } else if (key === "o") {
    cyclePane(1);
  } else if (/^[1-9]$/.test(key)) {
    const layout = viewLayout();
    const t = layout && layout.tabs[Number(key) - 1];
    if (t) selectTab(t.id);
  } else if (key === ",") {
    const t = activeTab();
    if (t) {
      tabRenaming = t.id;
      renderTabs();
    }
  } else if (key === "&") {
    const t = activeTab();
    if (t) closeTab(t.id);
  } else if (key === "?") {
    $("#dlg-keys").showModal();
  }
  return true;
}

// Capture phase: runs before xterm sees the key, so hub shortcuts never
// reach the shell (Ctrl+K twice sends a literal ^K).
window.addEventListener(
  "keydown",
  (event) => {
    const mod = event.ctrlKey || event.metaKey;
    if (mod && !event.altKey && !event.shiftKey && event.code === "KeyK") {
      event.preventDefault();
      event.stopPropagation();
      if (paletteOpen()) {
        closePalette();
        if (focusedPane()) focusedPane().send(JSON.stringify({ type: "input", data: "\x0b" }));
      } else if (!document.querySelector("dialog[open]")) {
        search.close(false);
        openPalette();
      }
      return;
    }
    // Ctrl Shift F (Cmd Shift F on macOS): search every session.
    if (mod && event.shiftKey && !event.altKey && event.code === "KeyF") {
      event.preventDefault();
      event.stopPropagation();
      if (paletteOpen()) closePalette();
      if (!document.querySelector("dialog[open]")) search.toggle();
      return;
    }
    if (paletteOpen() || search.isOpen() || document.querySelector("dialog[open]") || $("#ctx-menu")) return;
    if (prefixKey(event)) return;
    if (!hubChord(event)) return;
    let handled = true;
    const digit = /^Digit([1-9])$/.exec(event.code);
    const letter = /^Key([A-Z])$/.exec(event.code);
    if ((digit || letter) && !IS_MAC && !typesItself(event, (digit || letter)[1])) {
      // The layout made Alt+key into a character of its own: the
      // terminal's, not a hub shortcut.
      handled = false;
    } else if (digit) {
      const target = attachable()[Number(digit[1]) - 1];
      if (target) openSession(target.id);
      else handled = false;
    } else if (event.code === "KeyN") {
      if (event.shiftKey) openWorkspaceDialog();
      else openSessionDialog();
    } else if (event.code === "ArrowDown" && !event.shiftKey) {
      jumpBy(1);
    } else if (event.code === "ArrowUp" && !event.shiftKey) {
      jumpBy(-1);
    } else if (event.code === "KeyR" && !event.shiftKey && state.attached) {
      startRename(state.attached);
    } else if (event.code === "KeyW" && !event.shiftKey && state.attached) {
      closeSession(state.attached);
    } else if (event.code === "KeyC") {
      if (event.shiftKey) commands.copyLast();
      else {
        review.close();
        commands.toggle();
      }
    } else if (event.code === "Backquote" && !event.shiftKey && state.attached) {
      commands.flipCompanion();
    } else if (event.code === "KeyF" && !event.shiftKey) {
      flywheel.togglePill();
    } else if (event.code === "KeyG" && !event.shiftKey) {
      review.togglePill();
    } else if (event.code === "Backslash" && !event.shiftKey && (IS_MAC || event.key === "\\")) {
      splitWithShell(focusedPane(), "right");
    } else if (event.code === "Minus" && !event.shiftKey && (IS_MAC || event.key === "-")) {
      splitWithShell(focusedPane(), "down");
    } else if (event.shiftKey && ARROWS[event.code]) {
      focusToward(ARROWS[event.code]);
    } else if (event.shiftKey && VIM[event.code]) {
      nudgePane(VIM[event.code]);
    } else if (event.code === "KeyZ" && !event.shiftKey) {
      toggleZoom();
    } else if (event.code === "KeyX" && !event.shiftKey && state.focusId && !phone()) {
      closePane();
    } else if (event.code === "KeyT" && !event.shiftKey && state.view && !phone()) {
      newTab();
    } else if (event.code === "PageDown" || event.code === "PageUp") {
      cycleTab(event.code === "PageDown" ? 1 : -1);
    } else {
      handled = false;
    }
    if (handled) {
      event.preventDefault();
      event.stopPropagation();
    }
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
  machines: () => {
    document.body.classList.remove("side-open");
    machines.openDialog();
  },
};
document.addEventListener("click", (event) => {
  const target = event.target.closest("[data-action]");
  if (target && actions[target.dataset.action]) actions[target.dataset.action]();
});

// flywheel hub: the phone. Everything that makes a terminal usable by
// thumb, created by app-layout.js with its helpers:
//
// * Viewport: the app is sized to the visual viewport, so the on-screen
//   keyboard shrinks the terminal instead of covering it or scrolling the
//   page (iOS) -- no layout jump, no zoom on focus.
// * Key bar above the keyboard: Esc, Tab, Shift+Tab (Claude's
//   permission-mode cycle), sticky Ctrl and Alt, arrows (held: repeat),
//   Enter, ^C, a few symbols phone keyboards hide, font size, Compose.
// * Quick answers while the attached agent is blocked: the options of the
//   prompt on screen (Claude's numbered permission dialogs, a "(y/n)"
//   line) as buttons.
// * Compose: a real text area (autocorrect, dictation) whose text is sent
//   as one bracketed paste plus Enter; per-device history.
// * Touch on the terminal: swipe scrolls (with momentum; in full-screen
//   programs it scrolls them), long-press selects (drag to extend) with a
//   Copy button, pinch sets the font size (kept per device).
// * A row floor (phones only): the PTY is shared by every viewer and sized
//   by the last one to resize, so a short phone viewport (keyboard up) never
//   shrinks it below MIN_ROWS; the terminal is then clipped around the
//   cursor. A desktop pane fits its box exactly (a short split pane must
//   not hide its top rows under the title bar).
"use strict";

// Options of the prompt lowest on a terminal's screen (`lines`: its
// logical lines): a numbered list (Claude's permission dialogs and
// questions: "❯ 1. Yes", "2. No, ..."), or a "(y/n)" style line -- whichever
// comes last, so an answered dialog still on screen above a new y/n
// question is not offered. Returns {question, options:[{label, seq}]} or
// null. Pure (no DOM): FwHubPrompt.parse.
window.FwHubPrompt = (function () {
  const OPTION = /^[\s│┃|]*(?:[❯›>▶➤●*]\s*)?(\d{1,2})[.)]\s+(.+?)[\s│┃|]*$/;
  const HOTKEY = /\s*\(([a-z])\)\s*$/i;
  const HINT = /\s*\((?:esc|shift\+tab|tab|enter|ctrl\+[a-z])\)\s*$/i;
  const YES_NO = /[[(]\s*(y\/n|yes\/no)\s*[\])]/i;

  // The last run numbered 1, 2, 3, ... with at most a few wrapped lines
  // between options.
  function numberedRun(lines) {
    const found = [];
    lines.forEach((text, index) => {
      const match = OPTION.exec(text);
      if (match) found.push({ n: Number(match[1]), text: match[2], index });
    });
    let run = [];
    for (let i = found.length - 1; i >= 0; i--) {
      const option = found[i];
      if (!run.length) {
        run = [option];
        continue;
      }
      const next = run[0];
      if (option.n === next.n - 1 && next.index - option.index <= 4) run.unshift(option);
      else if (run[0].n === 1) break;
      else run = [option];
    }
    return run.length >= 2 && run[0].n === 1 ? run : null;
  }

  // The index of a "(y/n)" line among the last six non-empty lines.
  function yesNoLine(lines) {
    let seen = 0;
    for (let i = lines.length - 1; i >= 0 && seen < 6; i--) {
      if (!lines[i].trim()) continue;
      seen += 1;
      if (YES_NO.test(lines[i])) return i;
    }
    return -1;
  }

  function parse(lines) {
    const run = numberedRun(lines);
    const yn = yesNoLine(lines);
    if (yn >= 0 && (!run || yn > run[run.length - 1].index)) {
      return {
        question: lines[yn].trim(),
        options: [{ label: "y  yes", seq: "y\r" }, { label: "n  no", seq: "n\r" }],
      };
    }
    if (!run) return null;
    let question = "";
    for (let i = run[0].index - 1; i >= Math.max(0, run[0].index - 4); i--) {
      const text = lines[i].replace(/[─-╿|]/g, "").trim();
      if (text) {
        question = text;
        break;
      }
    }
    // Programs that wrap their own lines (Claude's TUI) continue an
    // option on indented lines before the next one.
    run.forEach((option, i) => {
      const next = run[i + 1];
      if (!next) return;
      for (let j = option.index + 1; j < next.index; j++) {
        const more = lines[j].replace(/[│┃|]/g, "").trim();
        if (more) option.text += ` ${more}`;
      }
    });
    return {
      question,
      options: run.map(({ n, text }) => {
        const hotkey = HOTKEY.exec(text);
        const label = text.replace(HOTKEY, "").replace(HINT, "").trim();
        return { label: `${n}  ${label}`, seq: hotkey ? hotkey[1].toLowerCase() : String(n) };
      }),
    };
  }

  return { parse };
})();

window.FwHubMobile = function (ctx) {
  const { $, el, state, store, api, toast, copyText, sessionById, agentStatus } = ctx;
  // The focused pane's terminal (a phone shows one pane).
  const term = () => ctx.term();

  const coarse = window.matchMedia("(pointer: coarse)");
  const FONT_MIN = 8;
  const FONT_MAX = 24;
  const HISTORY_MAX = 50;
  // Fewest rows this page sizes the shared PTY to (Claude's permission
  // dialogs need about this many to draw).
  const MIN_ROWS = 16;

  // --- preferences -----------------------------------------------------

  // Phones get a smaller default so agent TUIs have room to draw.
  const defaultFont = () => (Math.min(window.screen.width, window.innerWidth) < 500 ? 11 : 13);
  const fontSize = () => {
    const kept = ctx.prefs.get("appearance.font_size", null);
    return Number.isInteger(kept) && kept >= FONT_MIN && kept <= FONT_MAX ? kept : defaultFont();
  };
  // "auto" (touch screens), "on", or "off".
  const keybarPref = () => {
    const kept = ctx.prefs.get("keyboard.key_bar", "auto");
    return ["auto", "on", "off"].includes(kept) ? kept : "auto";
  };
  const keybarWanted = () => keybarPref() === "on" || (keybarPref() === "auto" && coarse.matches);

  // --- viewport --------------------------------------------------------

  // The visual viewport is what is left above the on-screen keyboard. The
  // app is pinned to it (CSS --app-top / --app-height), so the terminal
  // refits into that space and the page itself never scrolls.
  const viewport = window.visualViewport;
  let lastHeight = 0;
  function syncViewport() {
    const root = document.documentElement;
    const height = viewport ? viewport.height : window.innerHeight;
    const top = viewport ? viewport.offsetTop : 0;
    root.style.setProperty("--app-height", `${Math.round(height)}px`);
    root.style.setProperty("--app-top", `${Math.round(top)}px`);
    // A keyboard: the visual viewport is well short of the layout one.
    const keyboard = window.innerHeight - height > 120 || (coarse.matches && lastHeight - height > 120);
    document.body.classList.toggle("kb-open", keyboard && coarse.matches);
    if (!keyboard) lastHeight = Math.max(lastHeight, height);
    // iOS scrolls the page to show a focused field; the app is fixed, so
    // put the page back.
    if (window.scrollY || window.scrollX) window.scrollTo(0, 0);
  }
  if (viewport) {
    viewport.addEventListener("resize", syncViewport);
    viewport.addEventListener("scroll", syncViewport);
  }
  window.addEventListener("resize", syncViewport);
  window.addEventListener("orientationchange", () => {
    lastHeight = 0;
    setTimeout(syncViewport, 250);
  });
  syncViewport();

  // --- input: sticky modifiers ----------------------------------------

  const mods = { ctrl: 0, alt: 0 }; // 0 off, 1 next key, 2 locked

  function controlChar(ch) {
    if (/^[a-zA-Z]$/.test(ch)) return String.fromCharCode(ch.toUpperCase().charCodeAt(0) & 0x1f);
    const table = { "@": "\x00", " ": "\x00", "[": "\x1b", "\\": "\x1c", "]": "\x1d", "^": "\x1e", "_": "\x1f", "?": "\x7f", "2": "\x00", "3": "\x1b", "4": "\x1c", "5": "\x1d", "6": "\x1e", "7": "\x1f", "8": "\x7f" };
    return table[ch] ?? ch;
  }

  function release() {
    let changed = false;
    for (const key of ["ctrl", "alt"]) {
      if (mods[key] === 1) {
        mods[key] = 0;
        changed = true;
      }
    }
    if (changed) renderMods();
  }

  // What the terminal typed (soft keyboard), with armed modifiers applied
  // to its first character.
  function transformInput(data) {
    if (!data || (!mods.ctrl && !mods.alt)) return data;
    const chars = Array.from(data);
    let first = chars[0];
    if (mods.ctrl) first = controlChar(first);
    if (mods.alt) first = `\x1b${first}`;
    release();
    return first + chars.slice(1).join("");
  }

  // Cursor keys honour the program's cursor-key mode and modifiers.
  function arrow(letter) {
    const mod = 1 + (mods.alt ? 2 : 0) + (mods.ctrl ? 4 : 0);
    if (mod > 1) return `\x1b[1;${mod}${letter}`;
    const app = term() && term().modes && term().modes.applicationCursorKeysMode;
    return `${app ? "\x1bO" : "\x1b["}${letter}`;
  }

  function send(data) {
    if (!state.attached || state.exited) return;
    ctx.sendData(data);
  }

  // --- key bar ---------------------------------------------------------

  // The first row holds what driving an agent takes most (menus: arrows
  // and Enter; Esc, ^C, Shift+Tab) and always fits a phone's width; the
  // second scrolls sideways.
  const PRIMARY = [
    { label: "esc", title: "Escape", seq: () => "\x1b" },
    { label: "tab", title: "Tab", seq: () => (mods.alt ? "\x1b\t" : "\t") },
    { label: "⇧tab", title: "Shift+Tab (Claude: cycle permission mode)", seq: () => "\x1b[Z" },
    { label: "ctrl", title: "Ctrl (sticky: tap for the next key, twice to lock)", mod: "ctrl" },
    { label: "←", title: "Left", seq: () => arrow("D"), repeat: true },
    { label: "↑", title: "Up", seq: () => arrow("A"), repeat: true },
    { label: "↓", title: "Down", seq: () => arrow("B"), repeat: true },
    { label: "→", title: "Right", seq: () => arrow("C"), repeat: true },
    { label: "↵", title: "Enter", seq: () => (mods.alt ? "\x1b\r" : "\r"), wide: true },
    { label: "^c", title: "Ctrl+C (interrupt)", seq: () => "\x03", danger: true },
  ];
  const SECONDARY = [
    { label: "alt", title: "Alt (sticky: tap for the next key, twice to lock)", mod: "alt" },
    { label: "/", title: "Slash", seq: () => "/" },
    { label: "|", title: "Pipe", seq: () => "|" },
    { label: "~", title: "Tilde", seq: () => "~" },
    { label: "-", title: "Dash", seq: () => "-" },
    { label: "pgup", title: "Page Up", seq: () => "\x1b[5~", repeat: true },
    { label: "pgdn", title: "Page Down", seq: () => "\x1b[6~", repeat: true },
  ];

  function keyButton(key) {
    const button = el("button", {
      type: "button",
      className: `kb-key${key.mod ? " mod" : ""}${key.wide ? " wide" : ""}${key.danger ? " danger" : ""}`,
      textContent: key.label,
      title: key.title,
      "aria-label": key.title,
      dataset: key.mod ? { mod: key.mod } : {},
    });
    if (key.mod) {
      holdFocus(button);
      button.addEventListener("click", () => {
        // Off -> next key -> locked -> off.
        mods[key.mod] = (mods[key.mod] + 1) % 3;
        renderMods();
      });
      return button;
    }
    let delay = 0;
    let timer = 0;
    const stop = () => {
      clearTimeout(delay);
      clearInterval(timer);
      delay = 0;
      timer = 0;
      button.classList.remove("held");
    };
    const fire = () => {
      send(key.seq());
      ctx.onKey();
    };
    button.addEventListener("pointerdown", (event) => {
      // Keep the terminal (and its keyboard) focused.
      event.preventDefault();
      if (event.button !== 0) return;
      button.setPointerCapture(event.pointerId);
      button.classList.add("held");
      fire();
      if (key.repeat) {
        delay = setTimeout(() => {
          timer = setInterval(() => send(key.seq()), 60);
        }, 380);
      }
      release();
    });
    for (const type of ["pointerup", "pointercancel", "lostpointercapture"]) {
      button.addEventListener(type, stop);
    }
    // Keyboard activation (Enter/Space on the focused button).
    button.addEventListener("click", (event) => {
      if (event.detail === 0) {
        fire();
        release();
      }
    });
    return button;
  }

  // A tap on a bar button must not take focus from the terminal (that
  // would close the on-screen keyboard).
  function holdFocus(button) {
    button.addEventListener("pointerdown", (event) => event.preventDefault());
  }

  function toolButton(label, title, onclick, extra) {
    const button = el("button", {
      type: "button",
      className: `kb-key tool${extra ? ` ${extra}` : ""}`,
      textContent: label,
      title,
      "aria-label": title,
      onclick,
    });
    holdFocus(button);
    return button;
  }

  function renderMods() {
    document.querySelectorAll("#keybar [data-mod]").forEach((button) => {
      const level = mods[button.dataset.mod];
      button.classList.toggle("armed", level === 1);
      button.classList.toggle("locked", level === 2);
      button.setAttribute("aria-pressed", String(level > 0));
    });
  }

  function buildKeybar() {
    const bar = $("#keybar");
    const primary = el(
      "div",
      { className: "kb-row kb-primary", role: "toolbar", "aria-label": "Terminal keys" },
      PRIMARY.map(keyButton),
    );
    const more = el(
      "div",
      { className: "kb-keys", role: "toolbar", "aria-label": "More keys" },
      SECONDARY.map(keyButton),
      toolButton("a−", "Smaller text", () => setFont(fontSize() - 1)),
      toolButton("a+", "Larger text", () => setFont(fontSize() + 1)),
    );
    const tools = el(
      "div",
      { className: "kb-tools" },
      toolButton("kbd", "Show / hide the keyboard", toggleKeyboard, "kbd-toggle"),
      toolButton("compose", "Write a prompt (Compose)", () => openCompose(), "compose-btn"),
    );
    bar.replaceChildren(primary, el("div", { className: "kb-row kb-second" }, more, tools));
    renderMods();
  }

  function toggleKeyboard() {
    const t = term();
    if (!t) return;
    if (document.activeElement === t.textarea) t.blur();
    else t.focus();
  }

  // --- font size -------------------------------------------------------

  // Every pane's text (the size is per device, not per pane).
  function setFont(size, keep = true) {
    const next = Math.max(FONT_MIN, Math.min(FONT_MAX, Math.round(size)));
    for (const pane of ctx.panes()) {
      const t = pane.term;
      if (!t || t.options.fontSize === next) continue;
      t.options.fontSize = next;
      pane.refit();
    }
    // Text size from the key bar, a pinch, or the palette: this device's.
    if (keep) ctx.prefs.set("appearance.font_size", next, { device: true });
  }

  // --- fitting ---------------------------------------------------------

  // Fit a pane's terminal to its box, but never below MIN_ROWS: the PTY's
  // size is every viewer's, and a phone with its keyboard up (or Compose
  // open) would otherwise squeeze the agent's real terminal to a row or
  // two. A floored terminal is clipped around the cursor (panes.js).
  // Desktop panes are not floored: a short split pane is a layout
  // the user chose, and clipping it would hide its top rows.
  // `pane`: {term, fit, host} (panes.js); the focused one if none.
  function fitTerminal(pane = ctx.pane()) {
    const t = pane && pane.term;
    const f = pane && pane.fit;
    if (!t || !f) return;
    const dims = f.proposeDimensions();
    if (!dims || !Number.isFinite(dims.cols) || !Number.isFinite(dims.rows)) return;
    const cols = Math.max(2, dims.cols);
    const rows = ctx.phone() ? Math.max(MIN_ROWS, dims.rows) : Math.max(2, dims.rows);
    pane.host.classList.toggle("floored", rows > dims.rows);
    if (t.cols !== cols || t.rows !== rows) t.resize(cols, rows);
  }

  // --- quick answers ---------------------------------------------------

  // The screen as logical lines: a line the terminal soft-wrapped is
  // joined back onto the one it continues. `live` (quick answers): the
  // program's current screen, wherever the user has scrolled back to --
  // an old numbered list in the scrollback is not the prompt; else the
  // rows on view (Copy screen).
  function screenLines(live) {
    const t = term();
    if (!t) return [];
    const buffer = t.buffer.active;
    const top = live ? buffer.baseY : buffer.viewportY;
    const lines = [];
    for (let y = 0; y < t.rows; y++) {
      const line = buffer.getLine(top + y);
      const text = line ? line.translateToString(true) : "";
      if (line && line.isWrapped && lines.length) lines[lines.length - 1] += text;
      else lines.push(text);
    }
    return lines;
  }

  let quickKey = "";
  function renderQuick() {
    const bar = $("#quick");
    const session = sessionById(state.attached);
    // With the key bar (touch screens): a keyboard answers by itself, and
    // the bar takes rows from the terminal every viewer shares.
    const agent = session && !state.exited && keybarWanted() ? agentStatus(session) : null;
    if (!agent || agent.status !== "blocked") {
      if (!bar.hidden) {
        bar.hidden = true;
        quickKey = "";
        updateBar();
      }
      return;
    }
    const prompt = window.FwHubPrompt.parse(screenLines(true));
    const options = prompt ? prompt.options : [];
    const question = agent.detail || (prompt && prompt.question) || "waiting on you";
    const key = JSON.stringify([state.attached, question, options]);
    if (key === quickKey && !bar.hidden) return;
    quickKey = key;
    const answer = (seq) => () => {
      send(seq);
      ctx.onKey();
    };
    const buttons = options.map((option) => {
      const button = el("button", {
        type: "button",
        className: "qa",
        textContent: option.label,
        title: `Answer: ${option.label}`,
        onclick: answer(option.seq),
      });
      holdFocus(button);
      return button;
    });
    if (!options.length) {
      const enter = el("button", { type: "button", className: "qa", textContent: "↵  enter", onclick: answer("\r") });
      holdFocus(enter);
      buttons.push(enter);
    }
    const cancel = el("button", { type: "button", className: "qa-esc", textContent: "esc", title: "Escape (cancel)", onclick: answer("\x1b") });
    holdFocus(cancel);
    bar.replaceChildren(
      el(
        "div",
        { className: "qa-head" },
        el("div", { className: "qa-question" }, el("b", { textContent: "blocked" }), " ", question),
        cancel,
      ),
      el("div", { className: "qa-options" }, buttons),
    );
    bar.hidden = false;
    updateBar();
  }

  let quickTimer = 0;
  const scheduleQuick = () => {
    clearTimeout(quickTimer);
    quickTimer = setTimeout(renderQuick, 120);
  };

  // --- compose ---------------------------------------------------------

  const history = () => {
    const kept = store.get("compose-history", []);
    return Array.isArray(kept) ? kept.filter((t) => typeof t === "string") : [];
  };
  let historyAt = -1; // index into history while browsing, -1 = draft
  let draft = "";
  // The session the text area's text is for (its draft key).
  let composeFor = null;

  function openCompose() {
    if (!state.attached) return;
    const panel = $("#compose");
    panel.hidden = false;
    updateBar();
    document.body.classList.add("composing");
    const area = $("#compose-text");
    if (composeFor !== state.attached) {
      composeFor = state.attached;
      const saved = store.get(`draft.${composeFor}`, "");
      area.value = typeof saved === "string" ? saved : "";
      $("#compose-pos").textContent = "";
    }
    historyAt = -1;
    autosize();
    area.focus();
  }

  function closeCompose(refocus) {
    const panel = $("#compose");
    if (panel.hidden) return;
    panel.hidden = true;
    updateBar();
    document.body.classList.remove("composing");
    $("#compose-text").blur();
    if (refocus && term()) term().focus();
  }

  // Another session is attached: the text stays that session's draft (it
  // is kept as typed), and the panel closes rather than send it here.
  function switchCompose() {
    if (composeFor === null || composeFor === state.attached) return;
    closeCompose(false);
    composeFor = null;
    $("#compose-text").value = "";
    $("#compose-pos").textContent = "";
    historyAt = -1;
  }

  // Drafts of sessions that no longer exist (`ids`: every session).
  function pruneDrafts(ids) {
    const keep = new Set(ids);
    try {
      for (let i = window.localStorage.length - 1; i >= 0; i--) {
        const key = window.localStorage.key(i);
        if (key && key.startsWith("fw-hub.draft.") && !keep.has(key.slice("fw-hub.draft.".length))) {
          window.localStorage.removeItem(key);
        }
      }
    } catch (_) {
      // Storage unavailable: nothing was kept either.
    }
  }

  // Sign-out: what was typed into Compose leaves this device.
  function forget() {
    pruneDrafts([]);
    try {
      window.localStorage.removeItem("fw-hub.compose-history");
    } catch (_) {
      // As above.
    }
    composeFor = null;
    $("#compose-text").value = "";
  }

  function autosize() {
    const area = $("#compose-text");
    area.style.height = "auto";
    area.style.height = `${Math.min(area.scrollHeight + 2, Math.round(window.innerHeight * 0.4))}px`;
  }

  function recall(step) {
    const list = history();
    if (!list.length) return;
    const area = $("#compose-text");
    if (historyAt === -1) draft = area.value;
    historyAt = Math.max(-1, Math.min(list.length - 1, historyAt + step));
    area.value = historyAt === -1 ? draft : list[historyAt];
    autosize();
    area.setSelectionRange(area.value.length, area.value.length);
    $("#compose-pos").textContent = historyAt === -1 ? "" : `${historyAt + 1}/${list.length}`;
  }

  async function sendCompose(run) {
    const area = $("#compose-text");
    const text = area.value.replace(/\s+$/, "");
    const id = composeFor;
    if (!id || id !== state.attached || !text.trim()) {
      area.focus();
      return;
    }
    const button = run ? $("#compose-send") : $("#compose-paste");
    button.disabled = true;
    try {
      await api("POST", `/api/sessions/${id}/paste`, { text, run });
      const list = history().filter((t) => t !== text);
      list.unshift(text);
      store.set("compose-history", list.slice(0, HISTORY_MAX));
      area.value = "";
      store.set(`draft.${id}`, "");
      historyAt = -1;
      $("#compose-pos").textContent = "";
      autosize();
      closeCompose(false);
    } catch (error) {
      toast(`Not sent: ${error.message}`, { tone: "bad", timeout: 7000 });
    } finally {
      button.disabled = false;
    }
  }

  function bindCompose() {
    const area = $("#compose-text");
    area.addEventListener("input", () => {
      autosize();
      if (composeFor) store.set(`draft.${composeFor}`, area.value);
    });
    area.addEventListener("keydown", (event) => {
      if (event.isComposing) return;
      if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
        event.preventDefault();
        sendCompose(true);
      } else if (event.key === "Escape") {
        event.preventDefault();
        closeCompose(true);
      } else if (event.key === "ArrowUp" && area.selectionStart === 0 && area.selectionEnd === 0) {
        event.preventDefault();
        recall(1);
      } else if (event.key === "ArrowDown" && historyAt >= 0 && area.selectionStart === area.value.length) {
        event.preventDefault();
        recall(-1);
      }
    });
    $("#compose-send").addEventListener("click", () => sendCompose(true));
    $("#compose-paste").addEventListener("click", () => sendCompose(false));
    $("#compose-prev").addEventListener("click", () => recall(1));
    $("#compose-next").addEventListener("click", () => recall(-1));
    $("#compose-close").addEventListener("click", () => closeCompose(false));
  }

  // --- touch on the terminal -------------------------------------------

  // Cell geometry of the rendered screen.
  function cells() {
    const t = term();
    const screen = t && t.element && t.element.querySelector(".xterm-screen");
    if (!screen) return null;
    const rect = screen.getBoundingClientRect();
    if (!rect.width || !t.cols || !t.rows) return null;
    return { rect, width: rect.width / t.cols, height: rect.height / t.rows };
  }

  // Scroll by whole lines: the scrollback on the normal screen; a
  // full-screen program gets what a mouse wheel sends there (wheel
  // reports with mouse tracking, else cursor keys).
  function scrollLines(lines) {
    const t = term();
    if (!t || !lines) return;
    const alt = t.buffer.active.type === "alternate";
    const tracking = t.modes && t.modes.mouseTrackingMode !== "none";
    if (!alt && !tracking) {
      t.scrollLines(lines);
      return;
    }
    if (tracking) {
      const geo = cells();
      const screen = t.element.querySelector(".xterm-screen");
      if (!geo || !screen) return;
      for (let i = 0; i < Math.abs(lines); i++) {
        screen.dispatchEvent(new WheelEvent("wheel", {
          deltaY: Math.sign(lines) * geo.height,
          deltaMode: 0,
          clientX: geo.rect.left + geo.rect.width / 2,
          clientY: geo.rect.top + geo.rect.height / 2,
          bubbles: true,
          cancelable: true,
        }));
      }
      return;
    }
    const seq = arrow(lines < 0 ? "A" : "B");
    send(seq.repeat(Math.abs(lines)));
  }

  const touch = {
    mode: null, // "pending" | "scroll" | "select" | "pinch"
    startX: 0,
    startY: 0,
    lastX: 0,
    lastY: 0,
    lastT: 0,
    carry: 0, // px not yet scrolled
    velocity: 0, // px/ms
    hold: 0,
    anchor: null, // {col, row} where a selection started
    pinchFrom: 0,
    fontFrom: 0,
    momentum: 0,
  };

  function cellAt(x, y) {
    const geo = cells();
    const t = term();
    if (!geo) return null;
    const col = Math.max(0, Math.min(t.cols - 1, Math.floor((x - geo.rect.left) / geo.width)));
    const row = Math.max(0, Math.min(t.rows - 1, Math.floor((y - geo.rect.top) / geo.height)));
    return { col, row: row + t.buffer.active.viewportY };
  }

  // The word under a cell (non-space run), for a long-press.
  function wordAt(cell) {
    const line = term().buffer.active.getLine(cell.row);
    if (!line) return { start: cell.col, length: 1 };
    const text = line.translateToString(false);
    const isWord = (ch) => ch && !/\s/.test(ch);
    if (!isWord(text[cell.col])) return { start: cell.col, length: 1 };
    let start = cell.col;
    let end = cell.col;
    while (start > 0 && isWord(text[start - 1])) start--;
    while (end < text.length - 1 && isWord(text[end + 1])) end++;
    return { start, length: end - start + 1 };
  }

  function selectBetween(a, b) {
    const t = term();
    const [from, to] = a.row < b.row || (a.row === b.row && a.col <= b.col) ? [a, b] : [b, a];
    const length = (to.row - from.row) * t.cols + (to.col - from.col) + 1;
    t.select(from.col, from.row, length);
  }

  function stopMomentum() {
    cancelAnimationFrame(touch.momentum);
    touch.momentum = 0;
  }

  function flingFrom(velocity) {
    const geo = cells();
    if (!geo || Math.abs(velocity) < 0.15) return;
    let v = velocity;
    let last = performance.now();
    const step = (now) => {
      const dt = now - last;
      last = now;
      touch.carry += v * dt;
      const lines = Math.trunc(touch.carry / geo.height);
      if (lines) {
        touch.carry -= lines * geo.height;
        scrollLines(-lines);
      }
      v *= Math.pow(0.994, dt);
      if (Math.abs(v) > 0.05) touch.momentum = requestAnimationFrame(step);
      else touch.momentum = 0;
    };
    touch.momentum = requestAnimationFrame(step);
  }

  function bindTouch() {
    const area = $("#terminal");
    const distance = (e) =>
      Math.hypot(e.touches[0].clientX - e.touches[1].clientX, e.touches[0].clientY - e.touches[1].clientY);

    area.addEventListener("touchstart", (event) => {
      stopMomentum();
      hideSelectionPop();
      if (event.touches.length === 2) {
        clearTimeout(touch.hold);
        touch.mode = "pinch";
        touch.pinchFrom = distance(event);
        touch.fontFrom = fontSize();
        return;
      }
      if (event.touches.length !== 1) return;
      const point = event.touches[0];
      touch.mode = "pending";
      touch.startX = touch.lastX = point.clientX;
      touch.startY = touch.lastY = point.clientY;
      touch.lastT = event.timeStamp;
      touch.carry = 0;
      touch.velocity = 0;
      clearTimeout(touch.hold);
      touch.hold = setTimeout(() => {
        if (touch.mode !== "pending" || !term()) return;
        const cell = cellAt(touch.startX, touch.startY);
        if (!cell) return;
        touch.mode = "select";
        const word = wordAt(cell);
        touch.anchor = { col: word.start, row: cell.row };
        term().select(word.start, cell.row, word.length);
        if (navigator.vibrate) navigator.vibrate(10);
      }, 450);
    }, { passive: true, capture: true });

    area.addEventListener("touchmove", (event) => {
      if (!touch.mode) return;
      if (touch.mode === "pinch") {
        if (event.touches.length !== 2) return;
        event.preventDefault();
        const scale = distance(event) / touch.pinchFrom;
        setFont(touch.fontFrom * scale, false);
        return;
      }
      const point = event.touches[0];
      if (touch.mode === "pending") {
        if (Math.hypot(point.clientX - touch.startX, point.clientY - touch.startY) < 8) return;
        clearTimeout(touch.hold);
        touch.mode = "scroll";
      }
      event.preventDefault();
      event.stopPropagation();
      if (touch.mode === "select") {
        const cell = cellAt(point.clientX, point.clientY);
        if (cell && touch.anchor) selectBetween(touch.anchor, cell);
        return;
      }
      const dy = point.clientY - touch.lastY;
      const dt = Math.max(1, event.timeStamp - touch.lastT);
      touch.velocity = 0.8 * (dy / dt) + 0.2 * touch.velocity;
      touch.lastY = point.clientY;
      touch.lastT = event.timeStamp;
      const geo = cells();
      if (!geo) return;
      touch.carry += dy;
      const lines = Math.trunc(touch.carry / geo.height);
      if (lines) {
        touch.carry -= lines * geo.height;
        // Finger down = content down = back in the scrollback.
        scrollLines(-lines);
      }
    }, { passive: false, capture: true });

    const end = (event) => {
      clearTimeout(touch.hold);
      const mode = touch.mode;
      if (mode === "pinch") {
        if (event.touches.length) return;
        ctx.prefs.set("appearance.font_size", term().options.fontSize, { device: true });
      } else if (mode === "scroll") {
        // A recent move carries on as momentum.
        if (event.timeStamp - touch.lastT < 80) flingFrom(touch.velocity);
        event.preventDefault();
      } else if (mode === "select") {
        event.preventDefault();
        showSelectionPop(touch.lastX, touch.startY);
      }
      touch.mode = null;
    };
    area.addEventListener("touchend", end, { passive: false, capture: true });
    area.addEventListener("touchcancel", () => {
      clearTimeout(touch.hold);
      touch.mode = null;
    }, { capture: true });
    // Track x for the popover position.
    area.addEventListener("touchmove", (event) => {
      if (event.touches.length) touch.lastX = event.touches[0].clientX;
    }, { passive: true });
  }

  // --- selection popover -----------------------------------------------

  function showSelectionPop(x, y) {
    const t = term();
    if (!t || !t.hasSelection()) return;
    const pop = $("#sel-pop");
    const stage = $("#stage").getBoundingClientRect();
    pop.hidden = false;
    const width = pop.offsetWidth;
    const left = Math.max(4, Math.min(stage.width - width - 4, x - stage.left - width / 2));
    // Above the finger, or below it when there is no room above.
    const above = y - stage.top - pop.offsetHeight - 18;
    const top = above >= 4 ? above : Math.min(stage.height - pop.offsetHeight - 4, y - stage.top + 28);
    pop.style.left = `${left}px`;
    pop.style.top = `${top}px`;
  }

  function hideSelectionPop() {
    $("#sel-pop").hidden = true;
  }

  async function copy(text) {
    const ok = await copyText(text);
    toast(ok ? "Copied." : "Copy failed: select the text and use the system copy.", { tone: ok ? "" : "bad" });
  }

  function bindSelectionPop() {
    const pop = $("#sel-pop");
    pop.replaceChildren(
      el("button", {
        type: "button",
        className: "slab",
        textContent: "Copy",
        onclick: () => {
          const text = term().getSelection();
          hideSelectionPop();
          term().clearSelection();
          copy(text);
        },
      }),
      el("button", {
        type: "button",
        className: "slab ghost",
        textContent: "Copy screen",
        onclick: () => {
          hideSelectionPop();
          term().clearSelection();
          copy(screenLines(false).join("\n").replace(/\s+$/, ""));
        },
      }),
      el("button", {
        type: "button",
        className: "slab ghost",
        textContent: "×",
        title: "Clear selection",
        "aria-label": "Clear selection",
        onclick: () => {
          hideSelectionPop();
          term().clearSelection();
        },
      }),
    );
  }

  // --- rendering -------------------------------------------------------

  // The bar under the terminal shows while it has anything to show.
  function updateBar() {
    const empty = $("#keybar").hidden && $("#compose").hidden && $("#quick").hidden;
    $("#mobilebar").hidden = empty;
    placeToasts();
  }

  // Toasts rise from just above the bar (quick answers, compose, keys), not
  // over it: a blocked agent's answers stay tappable while one shows.
  // Every render calls this: with the bar hidden (a desktop) it reads no
  // geometry (each read forces a layout of the page just re-rendered), and
  // an unchanged value is not written (a root custom property restyles
  // the whole document).
  let barAbove = null;
  function placeToasts() {
    const bar = $("#mobilebar");
    let above = 0;
    if (!bar.hidden) {
      const top = bar.getBoundingClientRect().top;
      const viewport = window.visualViewport ? window.visualViewport.height : window.innerHeight;
      above = top > 0 ? Math.max(0, Math.round(viewport - top)) : 0;
    }
    if (above === barAbove) return;
    barAbove = above;
    document.documentElement.style.setProperty("--bar-above", `${above}px`);
  }
  if (window.ResizeObserver) new ResizeObserver(placeToasts).observe($("#mobilebar"));
  if (window.visualViewport) window.visualViewport.addEventListener("resize", placeToasts);
  window.addEventListener("resize", placeToasts);

  function render() {
    const session = sessionById(state.attached);
    const running = !!(session && session.live && session.state === "running" && !state.exited);
    const show = running && keybarWanted();
    document.body.classList.toggle("keybar-on", show);
    $("#keybar").hidden = !show;
    switchCompose();
    if (!running) closeCompose(false);
    renderQuick();
    updateBar();
  }

  const isBlocked = (session) => {
    const agent = session ? agentStatus(session) : null;
    return !!agent && agent.status === "blocked";
  };

  function paletteItems(cmd) {
    if (state.attached) cmd("Compose a prompt", "multi-line, sent as one paste + Enter", () => openCompose(), "pen");
    const pref = keybarPref();
    const next = { auto: keybarWanted() ? "off" : "on", on: "off", off: "auto" }[pref];
    cmd(`Key bar: ${next}`, `now ${pref}${pref === "auto" ? (coarse.matches ? " (touch: shown)" : " (shown on touch screens)") : ""}`, () => {
      ctx.prefs.set("keyboard.key_bar", next, { device: true });
      render();
    }, "gear");
    cmd("Text: larger", `${fontSize()}px`, () => setFont(fontSize() + 1), "plus");
    cmd("Text: smaller", `${fontSize()}px`, () => setFont(fontSize() - 1), "chev");
  }

  // Called for each pane's terminal once it exists: quick answers follow
  // the focused one's screen.
  function attachTerminal(t) {
    t.onWriteParsed(() => {
      if (t !== term()) return;
      if (!$("#quick").hidden || isBlocked(sessionById(state.attached))) scheduleQuick();
    });
  }

  // Touch gestures on the stage act on the focused pane's terminal.
  bindTouch();
  buildKeybar();
  bindCompose();
  bindSelectionPop();
  coarse.addEventListener("change", () => render());

  return {
    fontSize, setFont, fitTerminal, transformInput, attachTerminal, render, paletteItems, openCompose,
    pruneDrafts, forget,
  };
};

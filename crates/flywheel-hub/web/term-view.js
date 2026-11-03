// flywheel hub: a watch-only terminal for the share link viewer
// (viewer.js) and the replay player (player.js). An xterm.js terminal
// that never takes input, drawn at the size it is told (the session's,
// never the window's). Two ways to draw it: "fit" scales the font so that
// size fits the stage's width (the whole screen, however small), "read"
// keeps the font readable and lets the stage scroll (a phone on a wide
// PTY). "auto" picks fit unless that would be unreadable; a toggle
// (`toggleMode`, remembered per browser) overrides it. Resizes are queued
// behind the output written before them, so nothing is drawn at the wrong
// size.
"use strict";

window.FwHubTermView = function (host, stage) {
  const THEMES = {
    dark: {
      background: "#0c0d0c", foreground: "#e6e0cf", cursor: "#ff5a1f", cursorAccent: "#0c0d0c",
      selectionBackground: "rgba(255, 90, 31, 0.3)",
      black: "#22241f", red: "#ef5a4c", green: "#a3c76d", yellow: "#e3b341", blue: "#6fa3d6",
      magenta: "#c98bd1", cyan: "#62c2b5", white: "#d9d3c3", brightBlack: "#5d5a51",
      brightRed: "#ff7a66", brightGreen: "#bfe08a", brightYellow: "#f4cd68", brightBlue: "#93bff0",
      brightMagenta: "#e0a9e6", brightCyan: "#86ddd0", brightWhite: "#f6f1e4",
    },
    light: {
      background: "#f2eee4", foreground: "#23241f", cursor: "#e2470c", cursorAccent: "#f2eee4",
      selectionBackground: "rgba(226, 71, 12, 0.22)",
      black: "#23241f", red: "#bf3324", green: "#4b7419", yellow: "#8f6300", blue: "#245e9c",
      magenta: "#863890", cyan: "#0f7066", white: "#c9c2b0", brightBlack: "#6f6b60",
      brightRed: "#dc4430", brightGreen: "#5b8a22", brightYellow: "#a87700", brightBlue: "#2f72ba",
      brightMagenta: "#9f48aa", brightCyan: "#168a7e", brightWhite: "#fffdf7",
    },
  };
  const MAX_FONT = 15;
  // Fit mode never goes below this (it would not fit at all).
  const MIN_FONT = 3;
  // Auto mode reads instead of fitting below this.
  const READABLE = 8;
  // The font in read mode.
  const READ_FONT = 12;
  const MODE_KEY = "fw-hub-termview-mode";
  // JetBrains Mono's advance is 0.6 em; the measured width corrects it.
  const ADVANCE = 0.6;
  const light = window.matchMedia("(prefers-color-scheme: light)");
  const palette = () => (light.matches ? THEMES.light : THEMES.dark);

  const term = new window.Terminal({
    cols: 80,
    rows: 24,
    disableStdin: true,
    cursorBlink: false,
    scrollback: 10000,
    fontFamily: '"FW Term", ui-monospace, "SF Mono", Menlo, Consolas, monospace',
    fontSize: 13,
    theme: palette(),
    minimumContrastRatio: light.matches ? 4.5 : 1,
  });
  term.open(host);
  // Nothing typed here goes anywhere; keep the hidden textarea from
  // popping a phone's keyboard.
  const textarea = host.querySelector("textarea");
  if (textarea) {
    textarea.setAttribute("readonly", "");
    textarea.setAttribute("inputmode", "none");
    textarea.tabIndex = -1;
  }
  light.addEventListener("change", () => {
    term.options.theme = palette();
    term.options.minimumContrastRatio = light.matches ? 4.5 : 1;
  });

  let cols = 80;
  let rows = 24;
  let chosen = "auto";
  try {
    const saved = window.localStorage.getItem(MODE_KEY);
    if (saved === "fit" || saved === "read") chosen = saved;
  } catch (_) {
    // storage unavailable: auto
  }
  let drawn = "fit"; // what auto resolved to last
  stage.dataset.termMode = drawn;
  const listeners = [];

  // Scale the font so `cols` fit the stage's width, or keep it readable.
  function fit() {
    const style = window.getComputedStyle(host);
    const pad = parseFloat(style.paddingLeft) + parseFloat(style.paddingRight);
    const width = Math.max(0, stage.clientWidth - pad - 2);
    let size = Math.floor((width / (cols * ADVANCE)) * 10) / 10;
    size = Math.max(MIN_FONT, Math.min(MAX_FONT, size));
    const mode = chosen === "auto" ? (size < READABLE ? "read" : "fit") : chosen;
    if (mode !== drawn) {
      drawn = mode;
      stage.dataset.termMode = mode;
      if (mode === "fit") stage.scrollLeft = 0;
      listeners.forEach((fn) => fn(mode));
    }
    if (mode === "read") {
      size = Math.max(size, READ_FONT);
      if (term.options.fontSize !== size) term.options.fontSize = size;
      return;
    }
    if (term.options.fontSize !== size) term.options.fontSize = size;
    // Correct by the width actually drawn (the font's real advance).
    const screen = host.querySelector(".xterm-screen");
    if (screen && screen.offsetWidth > width && size > MIN_FONT) {
      const scaled = Math.max(MIN_FONT, Math.floor(size * (width / screen.offsetWidth) * 10) / 10);
      if (scaled !== size) term.options.fontSize = scaled;
    }
  }
  window.addEventListener("resize", fit);
  if (window.visualViewport) window.visualViewport.addEventListener("resize", fit);
  if (document.fonts) document.fonts.ready.then(fit);

  function applySize(c, r) {
    const nc = Math.max(2, c | 0);
    const nr = Math.max(1, r | 0);
    if (nc !== term.cols || nr !== term.rows) term.resize(nc, nr);
    cols = nc;
    rows = nr;
    fit();
  }

  return {
    term,
    // Output, in order.
    write(data, done) {
      term.write(data, done);
    },
    // Resize once what was written before has been drawn.
    resize(c, r) {
      term.write("", () => applySize(c, r));
    },
    // Resize at once (nothing written yet, or everything thrown away).
    resizeNow(c, r) {
      applySize(c, r);
    },
    // Clear everything, in order with the output (RIS).
    reset() {
      term.write("\x1bc");
    },
    size: () => ({ cols, rows }),
    fit,
    // "fit" or "read", as drawn now.
    mode: () => drawn,
    // Switch between fitting and reading (remembered); `fn(mode)` hears
    // every change.
    toggleMode() {
      chosen = drawn === "fit" ? "read" : "fit";
      try {
        window.localStorage.setItem(MODE_KEY, chosen);
      } catch (_) {
        // not remembered
      }
      fit();
    },
    onMode(fn) {
      listeners.push(fn);
      fn(drawn);
    },
  };
};

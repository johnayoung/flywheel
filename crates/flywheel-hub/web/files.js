// flywheel hub: files a session's terminal names. A path in the output
// (`src/main.rs:12`, `./run.sh`, `/abs/notes.md`, `Cargo.toml`) is a link
// once the hub says it is a file under the session's folders; clicking it
// opens the file viewer at that line. Protocol: POST
// /api/sessions/{id}/files/check (which candidates are files), GET
// /api/sessions/{id}/file?path= (the text). app-base.js creates it.
"use strict";

window.FwHubFiles = function (ctx) {
  const { el, icon, api, toast, copyText } = ctx;

  // --- finding paths in a line ---------------------------------------------

  // A path: an optional `~/`, `./`, `../`, or `/`, then segments, then an
  // optional `:line[:col]`. Not glued to a word, a dot, or a URL's `//`.
  const PATH = /(?<![\w.~/:@+-])((?:~\/|\.{1,2}\/|\/)?(?:[\w@+-][\w.@+-]*\/)*[\w@+-][\w.@+-]*)(?::(\d+)(?::\d+)?)?/g;
  // A file name with an extension, or one of the extensionless names
  // projects keep at their roots.
  const EXT = /\.[A-Za-z][A-Za-z0-9_-]{0,11}$/;
  const NAMES = new Set(["Makefile", "Dockerfile", "Justfile", "Procfile", "Gemfile", "Rakefile", "LICENSE", "Containerfile"]);

  function candidates(text) {
    const found = [];
    for (const match of text.matchAll(PATH)) {
      let path = match[1];
      // A sentence's full stop is not the extension's.
      const dots = path.match(/\.+$/);
      if (dots && !match[2]) path = path.slice(0, -dots[0].length);
      if (!path || /^\d+(\.\d+)*$/.test(path)) continue;
      const name = path.split("/").pop();
      if (!EXT.test(name) && !NAMES.has(name)) continue;
      const start = match.index;
      const end = start + (match[2] ? match[0].length : path.length);
      found.push({ path, line: match[2] ? Number(match[2]) : 0, start, end });
    }
    return found;
  }

  // The logical line (wrapped rows joined) holding buffer row `y` (1-based),
  // with the cell of each string index, so a link may span rows.
  function logicalLine(term, y) {
    const buffer = term.buffer.active;
    let first = y - 1;
    while (first > 0 && buffer.getLine(first)?.isWrapped) first--;
    let last = y - 1;
    while (buffer.getLine(last + 1)?.isWrapped) last++;
    let text = "";
    const cells = [];
    const cell = buffer.getNullCell();
    for (let row = first; row <= last; row++) {
      const line = buffer.getLine(row);
      if (!line) break;
      for (let x = 0; x < line.length; x++) {
        line.getCell(x, cell);
        if (cell.getWidth() === 0) continue; // a wide character's second half
        const chars = cell.getChars() || " ";
        for (let i = 0; i < chars.length; i++) {
          text += chars[i];
          cells.push({ x: x + 1, y: row + 1 });
        }
      }
    }
    return { text, cells };
  }

  // --- asking the hub which candidates are files ---------------------------

  const FOUND_MS = 60_000;
  const MISSING_MS = 10_000;
  const known = new Map(); // `${session}\0${path}` -> {found, at}
  const asking = new Map(); // same key -> Promise

  async function check(sessionId, paths) {
    const now = Date.now();
    const key = (p) => `${sessionId}\0${p}`;
    const fresh = (p) => {
      const k = known.get(key(p));
      return k && now - k.at < (k.found ? FOUND_MS : MISSING_MS);
    };
    const wanted = [...new Set(paths)].filter((p) => !fresh(p) && !asking.has(key(p))).slice(0, 64);
    if (wanted.length) {
      const request = api("POST", `/api/sessions/${sessionId}/files/check`, { paths: wanted })
        .then((answer) => {
          wanted.forEach((p, i) => known.set(key(p), { found: answer.files[i] || null, at: Date.now() }));
        })
        .catch(() => {
          // An older hub (no route) or a dropped link: no links for now.
          wanted.forEach((p) => known.set(key(p), { found: null, at: Date.now() }));
        })
        .finally(() => wanted.forEach((p) => asking.delete(key(p))));
      wanted.forEach((p) => asking.set(key(p), request));
    }
    await Promise.all(paths.map((p) => asking.get(key(p))).filter(Boolean));
    if (known.size > 2000) {
      for (const [k, v] of known) if (Date.now() - v.at > FOUND_MS) known.delete(k);
    }
    return new Set(paths.filter((p) => known.get(key(p))?.found));
  }

  // An xterm link provider for the terminal of the pane whose session is
  // `sessionOf()`.
  function linkProvider(term, sessionOf) {
    return {
      provideLinks(y, callback) {
        const sessionId = sessionOf();
        if (!sessionId) return callback(undefined);
        const { text, cells } = logicalLine(term, y);
        const found = candidates(text);
        if (!found.length) return callback(undefined);
        check(sessionId, found.map((f) => f.path)).then((files) => {
          if (sessionOf() !== sessionId) return callback(undefined);
          const links = found
            .filter((f) => files.has(f.path) && cells[f.start] && cells[f.end - 1])
            .map((f) => ({
              range: { start: cells[f.start], end: cells[f.end - 1] },
              text: text.slice(f.start, f.end),
              decorations: { underline: true, pointerCursor: true },
              activate: (event) => {
                if (event) event.preventDefault();
                open(sessionId, f.path, f.line);
              },
            }));
          callback(links.length ? links : undefined);
        });
      },
    };
  }

  // --- the viewer ------------------------------------------------------------

  const dialog = el("dialog", { className: "sheet fw-run-sheet fv-sheet", id: "dlg-file", "aria-labelledby": "fv-title" });
  document.body.append(dialog);
  const title = el("h2", { id: "fv-title" });
  const meta = el("span", { className: "fw-path fv-meta" });
  const body = el("div", { className: "run-body fv-body", tabIndex: 0 });
  let shownPath = "";
  dialog.append(
    el(
      "header",
      { className: "sheet-head run-head" },
      el("div", {}, el("span", { className: "sheet-kicker", textContent: "file" }), title, meta),
      el(
        "span",
        { className: "fv-tools" },
        el("button", {
          type: "button",
          className: "slab ghost fv-copy",
          textContent: "Copy path",
          onclick: () => copyText(shownPath).then((ok) => toast(ok ? "Path copied" : "Copy failed: this browser refused the clipboard.", { tone: ok ? "" : "bad" })),
        }),
        el("button", {
          type: "button",
          className: "icon",
          title: "Close (Esc)",
          "aria-label": "Close the file",
          onclick: () => dialog.close(),
        }, icon("x")),
      ),
    ),
    body,
  );
  dialog.addEventListener("click", (event) => {
    if (event.target === dialog) dialog.close();
  });

  const { bytes } = window.FwHubFmt;
  let seq = 0;

  async function open(sessionId, path, line) {
    const mine = ++seq;
    title.textContent = line ? `${path}:${line}` : path;
    meta.textContent = "";
    shownPath = path;
    body.replaceChildren(el("p", { className: "tx-note", textContent: "Reading…" }));
    if (!dialog.open) dialog.showModal();
    let file;
    try {
      file = await api("GET", `/api/sessions/${sessionId}/file?path=${encodeURIComponent(path)}`);
    } catch (error) {
      if (mine === seq) body.replaceChildren(el("p", { className: "tx-note bad", textContent: error.message }));
      return;
    }
    if (mine !== seq) return;
    shownPath = file.path;
    const lines = file.text.split("\n");
    if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
    meta.textContent = `${file.path} · ${bytes(file.size)} · ${lines.length} line${lines.length === 1 ? "" : "s"}${file.truncated ? ` · first ${bytes(file.text.length)} shown` : ""}`;
    const gutter = el("pre", { className: "fv-gutter", "aria-hidden": "true", textContent: lines.map((_, i) => i + 1).join("\n") });
    const code = el("pre", { className: "fv-code", textContent: lines.join("\n") });
    const target = line && line <= lines.length ? line : 0;
    const mark = target ? el("div", { className: "fv-mark", style: `--line:${target - 1}` }) : null;
    body.replaceChildren(el("div", { className: "fv-grid" }, mark, gutter, code));
    body.scrollTop = 0;
    if (target) {
      const height = parseFloat(getComputedStyle(code).lineHeight) || 18;
      body.scrollTop = Math.max(0, (target - 1) * height - body.clientHeight / 3);
    }
    body.focus();
  }

  return { linkProvider, open, isOpen: () => dialog.open };
};

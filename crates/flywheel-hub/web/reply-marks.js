// flywheel hub: copy buttons on an agent's replies inside its terminal.
// Claude Code starts each message with "● " (older versions "⏺ "), Codex
// with "• ", and indents the rest of it by two spaces. While the pointer
// is over an agent pane, the rows around it are read from xterm's buffer
// (soft wraps joined), each message's rows found from its bullet to the
// next row that is not indented, and a marker kept on each; the message
// under the pointer gets a copy button (an xterm decoration) at its first
// visible row's right edge. Tool calls ("● Bash(ls)" with "  ⎿" output,
// Codex's "• Ran ..." with "  └") get none.
//
// Copy takes the matching reply from the transcript (replies.js; the
// rows' letters and digits against the reply's match_key), exactly as
// written. A message with no match (its line not written yet) is copied
// as drawn: bullet and hanging indent removed, wrapped lines joined.
//
// Nothing is trusted across redraws: Claude Code reflows on resize and
// ctrl+o, so any write, scroll, or resize marks the found messages stale
// and the next pointer move reads the rows again (at most every
// RESCAN_MS, a bounded window: the viewport and LOOKBACK rows above it).
// Panes not hovered, hidden, or not an agent's do no work.
"use strict";

window.FwHubReplyMarks = function (ctx) {
  const { el, icon, sessionById } = ctx;
  // Rows above the viewport read to find where a message begins.
  const LOOKBACK = 200;
  // Least time between two reads of the rows.
  const RESCAN_MS = 250;
  // Messages tracked at once (the newest kept).
  const MAX_MARKS = 64;
  const BULLETS = { claude: /^[●⏺] (?=\S)/, codex: /^• (?=\S)/ };
  // A Claude Code tool call: a tool name and its argument list.
  const CLAUDE_TOOL = /^[●⏺] [A-Za-z][\w:.-]*(?: - [^(]*)?\(/;
  // A tool's output row, under its call.
  const TOOL_OUTPUT = /^ {2}[⎿└]/;
  // Rows that end a message though indented: a collapsed tool call
  // ("Read 1 file (ctrl+o to expand)"), a turn's footer, a rule.
  const NOT_TEXT = /\(ctrl\+o to expand\)$|^\s*(?:─{3,}|Worked for \d)/;
  // A list item's start (a wrapped line before it is not joined to it).
  const LIST_ITEM = /^\s*(?:[-*+•◦▪]|\d{1,9}[.)])\s/;

  // Follow `pane` (panes.js): `refresh()` after anything that may move or
  // redraw its rows, `reset()` when it shows another session, `dispose()`.
  function attach(pane) {
    const host = pane.host;
    let marks = []; // [{marker, rows, text, rendered}]
    let stale = true;
    let scannedAt = 0;
    let scanTimer = 0;
    let pointer = null; // the last pointer position over the pane {x, y}
    let shown = null; // {mark, line, decoration}
    let frame = 0;

    const term = () => pane.term;
    const kindOf = () => {
      const session = sessionById(pane.sessionId);
      return session && (session.kind === "claude" || session.kind === "codex") ? session.kind : null;
    };

    function clearShown() {
      if (shown) shown.decoration.dispose();
      shown = null;
    }

    function clearMarks() {
      clearShown();
      for (const mark of marks) mark.marker.dispose();
      marks = [];
    }

    // The rows of the active buffer from `from` to `to` (exclusive), soft
    // wraps joined: [{y, rows, text}].
    function logicalRows(buffer, from, to) {
      const out = [];
      // A row continuing one above the window: start at its first row.
      while (from > 0) {
        const line = buffer.getLine(from);
        if (!line || !line.isWrapped) break;
        from -= 1;
      }
      for (let y = from; y < to; y++) {
        const line = buffer.getLine(y);
        if (!line) break;
        const text = line.translateToString(true);
        const last = out[out.length - 1];
        if (line.isWrapped && last) {
          last.text += text;
          last.rows += 1;
        } else {
          out.push({ y, rows: 1, text: text.replace(/\s+$/, "") });
        }
      }
      for (const row of out) row.text = row.text.replace(/\s+$/, "");
      return out;
    }

    // Read the rows around the viewport and keep a marker per message.
    function scan() {
      const t = term();
      const kind = kindOf();
      clearMarks();
      stale = false;
      scannedAt = performance.now();
      if (!t || !kind || pane.disposed) return;
      ctx.replies.ensure(pane.sessionId);
      const buffer = t.buffer.active;
      const top = buffer.viewportY;
      const rows = logicalRows(buffer, Math.max(0, top - LOOKBACK), Math.min(buffer.length, top + t.rows));
      const bullet = BULLETS[kind];
      const cursor = buffer.baseY + buffer.cursorY;
      for (let i = 0; i < rows.length; i++) {
        if (!bullet.test(rows[i].text)) continue;
        let end = i + 1;
        while (end < rows.length && (rows[end].text === "" || rows[end].text.startsWith("  ")) && !NOT_TEXT.test(rows[end].text)) end++;
        while (end > i + 1 && rows[end - 1].text === "") end--;
        const region = rows.slice(i, end);
        const tool = (kind === "claude" && CLAUDE_TOOL.test(region[0].text)) ||
          (region[1] && TOOL_OUTPUT.test(region[1].text));
        if (tool) continue;
        const marker = t.registerMarker(region[0].y - cursor);
        if (!marker) continue;
        const lines = region.map((r, n) => (n === 0 ? r.text.replace(bullet, "") : r.text.replace(/^ {2}/, "")));
        marks.push({
          marker,
          rows: region.reduce((n, r) => n + r.rows, 0),
          lines,
          rendered: ctx.replies.reduce(lines.join("\n")),
        });
        i = end - 1;
      }
      while (marks.length > MAX_MARKS) marks.shift().marker.dispose();
    }

    // The wrapped lines of a message joined back: a line goes on the one
    // before it when that one had no room left for its first word (a hard
    // wrap at the pane's width) and it is not a new list item.
    function asDrawn(mark) {
      const width = Math.max(10, (term() ? term().cols : 80) - 2);
      const out = [];
      for (const line of mark.lines) {
        const prev = out[out.length - 1];
        const word = line.trim().split(/\s+/)[0] || "";
        const wrapped = prev !== undefined && prev.trim() && line.trim() && !LIST_ITEM.test(line) &&
          prev.length + 1 + word.length > width - 4;
        if (wrapped) out[out.length - 1] = `${prev} ${line.trim()}`;
        else out.push(line);
      }
      return out.join("\n").replace(/\s+$/, "");
    }

    async function copyMark(mark) {
      const reply = ctx.replies.match(pane.sessionId, mark.rendered);
      if (reply) {
        await ctx.replies.copy(reply.text, "(the reply's Markdown)");
      } else {
        await ctx.replies.copy(asDrawn(mark), "(as drawn: its transcript line is not read yet)");
      }
      if (pane.focus) pane.focus();
    }

    // The buffer row under the pointer, or null.
    function rowAt(point) {
      const t = term();
      const screen = t && t.element && t.element.querySelector(".xterm-screen");
      if (!screen || !t.rows) return null;
      const box = screen.getBoundingClientRect();
      if (point.x < box.left || point.x > box.right || point.y < box.top || point.y >= box.bottom) return null;
      return t.buffer.active.viewportY + Math.floor(((point.y - box.top) / box.height) * t.rows);
    }

    // Put the button on the message under the pointer (or take it away).
    function place() {
      frame = 0;
      const t = term();
      if (!t || !pointer || !kindOf() || pane.disposed || (ctx.paneHidden && ctx.paneHidden(pane))) {
        clearShown();
        return;
      }
      if (stale) {
        const wait = RESCAN_MS - (performance.now() - scannedAt);
        if (wait > 0) {
          // The button stays where it is until the rows are read again.
          clearTimeout(scanTimer);
          scanTimer = setTimeout(() => {
            scanTimer = 0;
            if (pointer) place();
          }, wait);
          return;
        }
        scan();
      }
      const row = rowAt(pointer);
      const mark = row === null
        ? null
        : marks.find((m) => !m.marker.isDisposed && m.marker.line >= 0 && row >= m.marker.line && row < m.marker.line + m.rows);
      if (!mark) {
        clearShown();
        return;
      }
      // Its first row in view: the button stays on screen for a long one.
      const buffer = t.buffer.active;
      const line = Math.max(mark.marker.line, buffer.viewportY);
      if (shown && shown.mark === mark && shown.line === line) return;
      clearShown();
      const marker = t.registerMarker(line - (buffer.baseY + buffer.cursorY));
      if (!marker) return;
      const decoration = t.registerDecoration({ marker, anchor: "right", width: 3, layer: "top" });
      if (!decoration) {
        marker.dispose();
        return;
      }
      decoration.onDispose(() => marker.dispose());
      const matched = !!ctx.replies.match(pane.sessionId, mark.rendered);
      decoration.onRender((node) => {
        if (node.firstChild) return;
        node.classList.add("reply-mark");
        node.append(
          el(
            "button",
            {
              type: "button",
              className: "reply-mark-btn",
              title: matched ? "Copy this reply (its Markdown, from the transcript)" : "Copy this reply as drawn (its transcript line is not read yet)",
              "aria-label": "Copy this reply",
              // Keep the terminal's focus and selection.
              onmousedown: (event) => {
                event.preventDefault();
                event.stopPropagation();
              },
              onclick: (event) => {
                event.preventDefault();
                event.stopPropagation();
                copyMark(mark);
              },
            },
            icon("copy"),
          ),
        );
      });
      shown = { mark, line, decoration };
    }

    function schedule() {
      if (!frame) frame = requestAnimationFrame(place);
    }

    const onMove = (event) => {
      pointer = { x: event.clientX, y: event.clientY };
      schedule();
    };
    const onLeave = () => {
      pointer = null;
      clearTimeout(scanTimer);
      clearShown();
    };
    host.addEventListener("mousemove", onMove, { passive: true });
    host.addEventListener("mouseleave", onLeave);

    return {
      // The rows may have changed: read them again before the next use.
      refresh() {
        stale = true;
        if (pointer) schedule();
      },
      reset() {
        stale = true;
        clearMarks();
      },
      dispose() {
        host.removeEventListener("mousemove", onMove);
        host.removeEventListener("mouseleave", onLeave);
        clearTimeout(scanTimer);
        cancelAnimationFrame(frame);
        clearMarks();
      },
    };
  }

  return { attach };
};

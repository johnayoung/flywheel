// flywheel hub: search across sessions (Ctrl Shift F, the palette, the
// header's search pill). One query box with literal / regex / case
// toggles and filters (workspace, session, kind, source); hits come back
// grouped by session with highlighted snippets. Opening a hit:
//   scrollback -> attach that session, scroll its terminal to the line and
//                 select the match (xterm's search addon);
//   transcript -> the transcript viewer at that message;
//   commands   -> the Commands drawer at that block.
// app.js creates it with its helpers; protocol: GET /api/search,
// GET /api/sessions/{id}/transcript (crates/flywheel-hub/src/search.rs).
"use strict";

window.FwHubSearch = function (ctx) {
  const { $, el, icon, api, state, sessionById, workspaceById, age, toast } = ctx;

  const SOURCES = ["scrollback", "transcript", "commands"];
  const SOURCE_LABEL = { scrollback: "terminal", transcript: "transcript", commands: "command" };
  const PER_SESSION = 8;
  // Hits per source fetched by each "more" of one session (paged with
  // the API's offset until the session has none left).
  const MORE_PAGE = 100;
  const DEBOUNCE_MS = 220;
  // How far from the expected line a terminal hit is looked for, when
  // output arrived since the search (logical lines).
  const DRIFT = 400;

  const ui = {
    open: false,
    regex: false,
    caseSensitive: false,
    ws: "",
    session: "",
    kind: "",
    sources: new Set(SOURCES),
    seq: 0,
    timer: 0,
    loading: false,
    error: "",
    answer: null,
    // session id -> {group, window, loading}: a session paged past the
    // first answer (`window`: hits per source fetched so far).
    expanded: {},
    selected: 0,
    // The parameters the answer shown was asked with.
    answeredFor: "",
  };

  const root = $("#search");
  const input = $("#search-input");
  const results = $("#search-results");
  const status = $("#search-status");

  // --- matching in the browser (highlights in the transcript viewer) ----

  const escapeRe = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

  // The query as a JS RegExp for highlighting, or null when it is not a
  // valid one here (Rust regex syntax differs at the edges: then only
  // the hit's own match is marked).
  function highlighter(query, regex, caseSensitive) {
    if (!query) return null;
    try {
      return new RegExp(regex ? query : escapeRe(query), caseSensitive ? "gu" : "giu");
    } catch (_) {
      return null;
    }
  }

  // `text` as nodes with every match of `re` (else of `literal`) marked.
  function marked(text, re, literal) {
    const out = [];
    let at = 0;
    const push = (start, end) => {
      if (start > at) out.push(text.slice(at, start));
      out.push(el("mark", { textContent: text.slice(start, end) }));
      at = end;
    };
    if (re) {
      re.lastIndex = 0;
      let m;
      let guard = 0;
      while ((m = re.exec(text)) && guard++ < 5000) {
        if (m[0].length === 0) {
          re.lastIndex++;
          continue;
        }
        push(m.index, m.index + m[0].length);
      }
    } else if (literal) {
      let i = text.indexOf(literal);
      while (i >= 0) {
        push(i, i + literal.length);
        i = text.indexOf(literal, i + literal.length);
      }
    }
    if (at < text.length) out.push(text.slice(at));
    return out;
  }

  // --- the overlay -----------------------------------------------------

  function setToggle(id, on) {
    $(id).setAttribute("aria-pressed", String(on));
  }

  function renderFilters() {
    const wsPick = $("#search-ws");
    wsPick.replaceChildren(
      el("option", { value: "", textContent: "all workspaces" }),
      ...state.workspaces.map((w) => el("option", { value: w.id, textContent: w.name, selected: w.id === ui.ws })),
    );
    if (ui.ws && !workspaceById(ui.ws)) ui.ws = "";
    const pool = state.sessions.filter((s) => !ui.ws || s.workspace_id === ui.ws);
    if (ui.session && !pool.some((s) => s.id === ui.session)) ui.session = "";
    $("#search-session").replaceChildren(
      el("option", { value: "", textContent: "all sessions" }),
      ...pool.map((s) => {
        const w = workspaceById(s.workspace_id);
        return el("option", { value: s.id, textContent: ui.ws || !w ? s.name : `${w.name} / ${s.name}`, selected: s.id === ui.session });
      }),
    );
    $("#search-kind").value = ui.kind;
    for (const source of SOURCES) $(`#search-src-${source}`).checked = ui.sources.has(source);
    setToggle("#search-regex", ui.regex);
    setToggle("#search-case", ui.caseSensitive);
  }

  function open(prefill) {
    ui.open = true;
    root.hidden = false;
    document.body.classList.remove("side-open");
    renderFilters();
    if (typeof prefill === "string" && prefill) input.value = prefill;
    input.focus();
    input.select();
    if (input.value.trim()) schedule(0);
    else render();
  }

  function close(refocus = true) {
    if (!ui.open) return;
    ui.open = false;
    root.hidden = true;
    clearTimeout(ui.timer);
    if (refocus) ctx.focusTerminal();
  }

  const toggle = () => (ui.open ? close() : open());

  function params(extra = {}) {
    const p = new URLSearchParams();
    p.set("q", input.value);
    if (ui.regex) p.set("regex", "true");
    if (ui.caseSensitive) p.set("case", "true");
    if (ui.ws) p.set("ws", ui.ws);
    if (ui.session) p.set("session", ui.session);
    if (ui.kind) p.set("kind", ui.kind);
    if (ui.sources.size && ui.sources.size < SOURCES.length) p.set("source", [...ui.sources].join(","));
    p.set("per_session", String(PER_SESSION));
    for (const [k, v] of Object.entries(extra)) p.set(k, String(v));
    return p;
  }

  function schedule(delay = DEBOUNCE_MS) {
    clearTimeout(ui.timer);
    ui.timer = setTimeout(run, delay);
  }

  async function run() {
    clearTimeout(ui.timer);
    ui.timer = 0;
    const query = input.value;
    const asked = params().toString();
    const seq = ++ui.seq;
    ui.expanded = {};
    if (!query.trim() || !ui.sources.size) {
      ui.answer = null;
      ui.answeredFor = "";
      ui.error = "";
      ui.loading = false;
      render();
      return;
    }
    ui.loading = true;
    renderStatus();
    try {
      const answer = await api("GET", `/api/search?${params()}`);
      if (seq !== ui.seq) return;
      ui.answer = answer;
      ui.answeredFor = asked;
      ui.error = "";
      ui.selected = 0;
    } catch (error) {
      if (seq !== ui.seq) return;
      ui.answer = null;
      ui.error = error.message;
    } finally {
      if (seq === ui.seq) ui.loading = false;
    }
    render();
  }

  const bySource = (hits) => SOURCES.flatMap((source) => hits.filter((h) => h.source === source));

  // The next page of one session's hits (MORE_PAGE per source). Each
  // source's hits come newest first and `offset` skips the ones already
  // shown, so the window is the same for every source -- unless the
  // first answer cut this group at the request's limit: then it starts
  // over from the newest.
  async function more(shown) {
    const id = shown.session_id;
    let entry = ui.expanded[id];
    if (!entry) {
      const counts = shown.counts;
      const whole = SOURCES.reduce((n, source) => n + Math.min(counts[source] || 0, PER_SESSION), 0);
      entry = whole === shown.hits.length && shown.hits.length
        ? { group: shown, window: PER_SESSION }
        : { group: { ...shown, hits: [] }, window: 0 };
      ui.expanded[id] = entry;
    }
    if (entry.loading) return;
    entry.loading = true;
    render();
    const seq = ui.seq;
    try {
      const answer = await api("GET", `/api/search?${params({ session: id, per_session: MORE_PAGE, offset: entry.window, limit: MORE_PAGE * SOURCES.length })}`);
      if (seq !== ui.seq) return;
      const page = answer.groups.find((g) => g.session_id === id);
      entry.window += MORE_PAGE;
      entry.group = page
        ? { ...page, hits: bySource([...entry.group.hits, ...page.hits]) }
        : { ...entry.group, more: false };
    } catch (error) {
      toast(error.message, { tone: "bad" });
    } finally {
      entry.loading = false;
    }
    if (seq === ui.seq) render();
  }

  function renderStatus() {
    const a = ui.answer;
    let text = "";
    if (ui.loading) text = "Searching…";
    else if (ui.error) text = ui.error;
    else if (!input.value.trim()) text = `Every terminal's scrollback, agent transcripts, and suggested commands. ${ui.regex ? "Regex (Rust syntax)." : "Literal text."} ${ui.caseSensitive ? "Case-sensitive." : "Any case."}`;
    else if (!ui.sources.size) text = "Pick at least one source.";
    else if (a) {
      const sessions = a.groups.filter((g) => g.hits.length).length;
      const counted = a.groups.length - sessions;
      text = a.hits
        ? `${a.hits} hit${a.hits === 1 ? "" : "s"} in ${sessions} session${sessions === 1 ? "" : "s"} · ${a.elapsed_ms} ms`
        : `No match in ${a.searched.sessions} session${a.searched.sessions === 1 ? "" : "s"} · ${a.elapsed_ms} ms`;
      if (a.truncated) {
        text += counted
          ? ` · limit reached: ${counted} more matching session${counted === 1 ? "" : "s"} counted below (show its hits, or narrow the search)`
          : " · limit reached (narrow the search)";
      }
      if (a.partial) text += " · time ran out: not every session was searched";
      if (a.notes && a.notes.length) text += ` · ${a.notes.join("; ")}`;
    }
    status.textContent = text;
    status.classList.toggle("bad", !!ui.error);
  }

  function hitPlace(hit) {
    if (hit.source === "scrollback") {
      return hit.from_end === 0 ? "last line" : `${hit.from_end} line${hit.from_end === 1 ? "" : "s"} up${hit.screen === "alternate" ? " (full screen)" : ""}`;
    }
    if (hit.source === "transcript") {
      const who = hit.kind === "tool" ? `tool ${hit.tool || ""}`.trim() : hit.role;
      return `#${hit.index} ${who}`;
    }
    return `#${hit.seq} ${hit.lang || (hit.shell ? "shell" : "text")}${hit.line ? ` · line ${hit.line + 1}` : ""}`;
  }

  // The flat list of hits shown, in order (keyboard navigation).
  let flat = [];

  function renderHit(group, hit, i) {
    const when = hit.timestamp ? age(hit.timestamp) : "";
    const extra = hit.matches > 1 ? `+${hit.matches - 1}` : "";
    return el(
      "li",
      {
        className: `search-hit src-${hit.source}${i === ui.selected ? " selected" : ""}`,
        role: "option",
        id: `search-hit-${i}`,
        "aria-selected": String(i === ui.selected),
        dataset: { index: String(i) },
        onclick: () => choose(i),
        onmousemove: () => {
          if (ui.selected !== i) select(i, false);
        },
      },
      el(
        "div",
        { className: "search-hit-meta" },
        el("span", { className: "search-src", textContent: SOURCE_LABEL[hit.source] }),
        el("span", { className: "search-place", textContent: hitPlace(hit) }),
        extra ? el("span", { className: "search-more", textContent: extra, title: `${hit.matches} matches here` }) : null,
        when ? el("span", { className: "search-when", textContent: when, title: hit.timestamp }) : null,
      ),
      // The match stays in view: the context before it gives way from
      // its left end, the rest of the line from its right.
      el(
        "div",
        { className: "search-snippet" },
        el("span", { className: "sn-before" }, el("span", { textContent: hit.before })),
        el("mark", { textContent: hit.match }),
        el("span", { className: "sn-after", textContent: hit.after }),
      ),
    );
  }

  function render() {
    renderStatus();
    flat = [];
    if (!ui.answer || !ui.open) {
      results.replaceChildren();
      return;
    }
    const nodes = [];
    for (const shown of ui.answer.groups) {
      const entry = ui.expanded[shown.session_id];
      const group = entry ? entry.group : shown;
      const session = sessionById(group.session_id);
      const counts = group.counts;
      const total = counts.scrollback + counts.transcript + counts.commands;
      const head = el(
        "header",
        { className: "search-group-head" },
        icon(ctx.glyphFor(group.kind)),
        el("span", { className: "search-ws", textContent: group.workspace }),
        el("b", { className: "search-session", textContent: group.session }),
        el("span", { className: `search-state st-${group.state}`, textContent: group.terminal ? group.state : `${group.state} · no terminal` }),
        el("span", { className: "search-count", textContent: `${total} match${total === 1 ? "" : "es"}` }),
      );
      const left = total - group.hits.length;
      const moreButton = group.more
        ? el("button", {
          type: "button",
          className: "text-btn search-more-btn",
          disabled: !!(entry && entry.loading),
          textContent: entry && entry.loading
            ? "loading…"
            : group.hits.length
              ? `more (${left} not shown)`
              : `past the limit: show its hits`,
          onclick: (event) => {
            event.stopPropagation();
            more(shown);
          },
        })
        : null;
      const items = group.hits.map((hit) => {
        const i = flat.length;
        flat.push({ group, hit });
        return renderHit(group, hit, i);
      });
      nodes.push(el(
        "section",
        { className: `search-group${session ? "" : " gone"}` },
        head,
        items.length ? el("ul", { className: "search-hits" }, items) : null,
        moreButton ? el("div", { className: "search-group-more" }, moreButton) : null,
      ));
    }
    if (!nodes.length) nodes.push(el("p", { className: "search-none", textContent: "Nothing found." }));
    results.replaceChildren(...nodes);
    if (ui.selected >= flat.length) ui.selected = Math.max(0, flat.length - 1);
    input.setAttribute("aria-activedescendant", flat.length ? `search-hit-${ui.selected}` : "");
  }

  function select(i, scroll = true) {
    ui.selected = Math.max(0, Math.min(flat.length - 1, i));
    for (const node of results.querySelectorAll(".search-hit")) {
      const on = node.dataset.index === String(ui.selected);
      node.classList.toggle("selected", on);
      node.setAttribute("aria-selected", String(on));
      if (on && scroll) node.scrollIntoView({ block: "nearest" });
    }
    input.setAttribute("aria-activedescendant", `search-hit-${ui.selected}`);
  }

  // --- opening hits ----------------------------------------------------

  async function choose(i) {
    const entry = flat[i];
    if (!entry) return;
    const { group, hit } = entry;
    if (!sessionById(group.session_id)) {
      toast(`${group.session} was removed.`, { tone: "bad" });
      return;
    }
    const query = { text: input.value, regex: ui.regex, caseSensitive: ui.caseSensitive };
    close(false);
    if (hit.source === "transcript") {
      openTranscript(group, hit.index, query);
    } else if (hit.source === "commands") {
      await ctx.openSession(group.session_id);
      ctx.openCommandsAt(group.session_id, hit.seq, !hit.shell);
    } else {
      await jumpToLine(group, hit);
    }
  }

  // The pane showing session `id` once its terminal has replayed.
  function paneReady(id, timeoutMs = 5000) {
    return new Promise((resolve) => {
      const started = Date.now();
      const look = () => {
        const pane = ctx.livePanes().find((p) => p.sessionId === id && p.term);
        if (pane && pane.drawn) {
          resolve(pane);
          return;
        }
        if (Date.now() - started > timeoutMs) {
          resolve(null);
          return;
        }
        setTimeout(look, 50);
      };
      look();
    });
  }

  // Logical lines of a terminal buffer, newest first, as the server
  // counts them: soft-wrapped rows joined, trailing blank lines skipped.
  function* logicalLines(buffer) {
    let y = buffer.length - 1;
    let started = false;
    while (y >= 0) {
      let top = y;
      while (top > 0 && buffer.getLine(top) && buffer.getLine(top).isWrapped) top--;
      let text = "";
      for (let r = top; r <= y; r++) {
        const line = buffer.getLine(r);
        if (line) text += line.translateToString(false);
      }
      text = text.replace(/\s+$/u, "");
      if (started || text) {
        started = true;
        yield { top, text };
      }
      y = top - 1;
    }
  }

  // The buffer row of the logical line holding the hit: the one
  // `from_end` lines up when it still holds the match, else the nearest
  // that does (output arrived since).
  function findLine(buffer, hit) {
    const want = hit.match_text;
    const candidates = [];
    let n = 0;
    for (const line of logicalLines(buffer)) {
      if (n > hit.from_end + DRIFT) break;
      if (line.text.includes(want)) candidates.push({ ...line, n });
      n++;
    }
    if (!candidates.length) return null;
    candidates.sort((a, b) => Math.abs(a.n - hit.from_end) - Math.abs(b.n - hit.from_end));
    return candidates[0];
  }

  // Keep toasts off a pane's banner (an exited session's Relaunch and
  // Close) for `ms`: they sit just above it instead of over it.
  let liftTimer = 0;
  function liftToastsAbove(banner, ms) {
    const toasts = $("#toasts");
    clearTimeout(liftTimer);
    if (!banner) {
      toasts.style.bottom = "";
      return;
    }
    const top = banner.getBoundingClientRect().top;
    toasts.style.bottom = `${Math.max(0, window.innerHeight - top + 8)}px`;
    liftTimer = setTimeout(() => {
      toasts.style.bottom = "";
    }, ms + 400);
  }

  async function jumpToLine(group, hit) {
    if (!group.terminal) {
      toast(`${group.session} has no terminal on this hub any more.`, { tone: "bad" });
      return;
    }
    await ctx.openSession(group.session_id);
    const pane = await paneReady(group.session_id);
    if (!pane) {
      toast(`Could not show ${group.session}'s terminal.`, { tone: "bad" });
      return;
    }
    const term = pane.term;
    const buffer = term.buffer.active;
    if (hit.screen === "primary" && buffer.type === "alternate") {
      toast(`${group.session} is showing a full-screen program; its scrollback is behind it until the program exits.`, { timeout: 6000 });
      return;
    }
    const line = findLine(buffer, hit);
    if (!line) {
      toast(el("span", {}, "That line is no longer in this terminal's scrollback: ", el("code", { className: "toast-code", textContent: `${hit.before}${hit.match}${hit.after}` })), { timeout: 7000 });
      return;
    }
    const addon = pane.search;
    const options = { caseSensitive: true };
    addon.clearDecorations();
    // Start at the line: the addon finds the next match from the
    // selection's start, then from each found match's end.
    term.select(0, line.top, 1);
    let found = addon.findNext(hit.match_text, options);
    // The nth occurrence on its line, when the line is where it was.
    const skip = line.n === hit.from_end ? hit.occurrence : 0;
    for (let k = 0; k < skip && found; k++) found = addon.findNext(hit.match_text, options);
    if (!found) {
      term.scrollToLine(line.top);
      return;
    }
    const step = (forward) => {
      if (forward) addon.findNext(hit.match_text, options);
      else addon.findPrevious(hit.match_text, options);
    };
    liftToastsAbove(pane.el.querySelector(".pane-banner:not([hidden])"), 8000);
    toast(el("span", {}, "Found in ", el("b", { textContent: group.session }), el("code", { className: "toast-code", textContent: hit.match_text })), {
      actions: [
        el("button", { type: "button", className: "text-btn", textContent: "previous", onclick: (e) => { e.stopPropagation(); step(false); } }),
        el("button", { type: "button", className: "text-btn", textContent: "next", onclick: (e) => { e.stopPropagation(); step(true); } }),
      ],
      timeout: 8000,
    });
  }

  // --- the transcript viewer ---------------------------------------------

  const viewer = {
    session: null,
    focus: null, // message index opened at
    messages: new Map(), // index -> message
    first: 0,
    next: 0,
    re: null,
    literal: "",
    loading: false,
  };

  async function loadPage(around, before, after) {
    const p = new URLSearchParams({ before: String(before), after: String(after) });
    if (around !== null) p.set("around", String(around));
    const page = await api("GET", `/api/sessions/${viewer.session.session_id}/transcript?${p}`);
    viewer.first = page.first;
    viewer.next = page.next;
    for (const m of page.messages) viewer.messages.set(m.index, m);
    return page;
  }

  async function openTranscript(group, index, query) {
    viewer.session = group;
    viewer.focus = index;
    viewer.messages = new Map();
    viewer.re = highlighter(query.text, query.regex, query.caseSensitive);
    viewer.literal = query.regex ? "" : query.text;
    const dialog = $("#dlg-transcript");
    $("#tx-title").textContent = group.session;
    $("#tx-meta").textContent = `${group.workspace} · ${group.kind} · ${group.state}${group.conversation ? ` · conversation ${group.conversation}` : ""}`;
    $("#tx-body").replaceChildren(el("p", { className: "tx-note", textContent: "Reading the transcript…" }));
    if (!dialog.open) dialog.showModal();
    try {
      const page = await loadPage(index, 25, 25);
      if (!page.transcript) {
        $("#tx-body").replaceChildren(el("p", { className: "tx-note", textContent: `${group.session} has written no transcript yet.` }));
        return;
      }
      $("#tx-meta").textContent += ` · ${page.transcript}`;
      renderTranscript(true);
    } catch (error) {
      $("#tx-body").replaceChildren(el("p", { className: "tx-note bad", textContent: error.message }));
    }
  }

  async function loadMore(earlier) {
    if (viewer.loading) return;
    viewer.loading = true;
    const indexes = [...viewer.messages.keys()];
    const body = $("#tx-body");
    const height = body.scrollHeight;
    try {
      if (earlier) await loadPage(Math.min(...indexes) - 1, 40, 0);
      else await loadPage(Math.max(...indexes) + 1, 0, 40);
      renderTranscript(false);
      // Keep what was in view in place when older messages went above.
      if (earlier) body.scrollTop += body.scrollHeight - height;
    } catch (error) {
      toast(error.message, { tone: "bad" });
    } finally {
      viewer.loading = false;
    }
  }

  function renderTranscript(scrollToFocus) {
    const body = $("#tx-body");
    const list = [...viewer.messages.values()].sort((a, b) => a.index - b.index);
    const nodes = [];
    if (list.length && list[0].index > viewer.first) {
      nodes.push(el("button", { type: "button", className: "slab ghost tx-more", textContent: `Earlier messages (${list[0].index - viewer.first})`, onclick: () => loadMore(true) }));
    } else if (viewer.first > 0) {
      nodes.push(el("p", { className: "tx-note", textContent: `The ${viewer.first} oldest messages are past what the hub keeps of this transcript.` }));
    }
    for (const m of list) {
      const who = m.kind === "tool" ? `tool · ${m.tool || ""}` : m.role;
      nodes.push(el(
        "article",
        { className: `tx-msg role-${m.role} kind-${m.kind}${m.index === viewer.focus ? " focus" : ""}`, dataset: { index: String(m.index) } },
        el(
          "header",
          { className: "tx-msg-head" },
          el("span", { className: "tx-who", textContent: who }),
          el("span", { className: "tx-index", textContent: `#${m.index}` }),
          m.timestamp ? el("span", { className: "tx-when", textContent: `${age(m.timestamp)} ago`, title: m.timestamp }) : null,
          m.truncated ? el("span", { className: "tx-cut", textContent: "cut at 64 KiB" }) : null,
        ),
        el("pre", { className: "tx-text" }, marked(m.text, viewer.re, viewer.literal)),
      ));
    }
    const last = list[list.length - 1];
    if (last && last.index < viewer.next - 1) {
      nodes.push(el("button", { type: "button", className: "slab ghost tx-more", textContent: `Later messages (${viewer.next - 1 - last.index})`, onclick: () => loadMore(false) }));
    }
    body.replaceChildren(...nodes);
    if (scrollToFocus) {
      const node = body.querySelector(".tx-msg.focus");
      if (node) {
        node.scrollIntoView({ block: "center" });
        const mark = node.querySelector("mark");
        if (mark) mark.scrollIntoView({ block: "center" });
      }
    }
  }

  // --- events ------------------------------------------------------------

  input.addEventListener("input", () => {
    const length = input.value.trim().length;
    // One character searches only on Enter (too many hits to be useful).
    if (length === 1) {
      clearTimeout(ui.timer);
      return;
    }
    schedule();
  });

  root.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      close();
      return;
    }
    if (event.altKey && !event.ctrlKey && !event.metaKey && (event.code === "KeyR" || event.code === "KeyC")) {
      event.preventDefault();
      event.stopPropagation();
      if (event.code === "KeyR") ui.regex = !ui.regex;
      else ui.caseSensitive = !ui.caseSensitive;
      renderFilters();
      schedule(0);
      return;
    }
    if (event.target.closest("select")) return;
    if (event.key === "ArrowDown" || (event.ctrlKey && event.key === "n")) {
      event.preventDefault();
      select(ui.selected + 1);
    } else if (event.key === "ArrowUp" || (event.ctrlKey && event.key === "p")) {
      event.preventDefault();
      select(ui.selected - 1);
    } else if (event.key === "PageDown") {
      event.preventDefault();
      select(ui.selected + 8);
    } else if (event.key === "PageUp") {
      event.preventDefault();
      select(ui.selected - 8);
    } else if (event.key === "Enter" && !event.target.closest("button")) {
      event.preventDefault();
      // Typed since the results came: search now (a one-character
      // query too); else open the picked hit.
      if (params().toString() !== ui.answeredFor) run();
      else choose(ui.selected);
    }
  });
  root.addEventListener("mousedown", (event) => {
    if (event.target === root) close();
  });
  $("#search-close").addEventListener("click", () => close());
  $("#search-regex").addEventListener("click", () => {
    ui.regex = !ui.regex;
    renderFilters();
    schedule(0);
    input.focus();
  });
  $("#search-case").addEventListener("click", () => {
    ui.caseSensitive = !ui.caseSensitive;
    renderFilters();
    schedule(0);
    input.focus();
  });
  $("#search-ws").addEventListener("change", (event) => {
    ui.ws = event.target.value;
    renderFilters();
    schedule(0);
  });
  $("#search-session").addEventListener("change", (event) => {
    ui.session = event.target.value;
    schedule(0);
  });
  $("#search-kind").addEventListener("change", (event) => {
    ui.kind = event.target.value;
    schedule(0);
  });
  for (const source of SOURCES) {
    $(`#search-src-${source}`).addEventListener("change", (event) => {
      if (event.target.checked) ui.sources.add(source);
      else ui.sources.delete(source);
      schedule(0);
    });
  }

  function paletteItems(cmd) {
    cmd("Search sessions", `Ctrl Shift F · scrollback, transcripts, commands`, () => open(), "search");
    const attached = sessionById(state.attached);
    if (attached) {
      cmd(`Search in ${attached.name}`, "this session only", () => {
        ui.ws = "";
        ui.session = attached.id;
        open();
      }, "search");
    }
  }

  // Sessions or workspaces changed: keep the pickers current.
  function refreshFilters() {
    if (ui.open) renderFilters();
  }

  return { open, close, toggle, isOpen: () => ui.open, paletteItems, refreshFilters };
};

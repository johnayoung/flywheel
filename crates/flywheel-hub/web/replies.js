// flywheel hub: agent replies. Every assistant text message of an agent
// session, read by the hub from its transcript (the Markdown exactly as
// written: no terminal wrapping, hanging indent, or bullet), split into
// blocks. "Copy last reply" (pane menu, palette, a key) copies the newest;
// the Commands drawer's Replies view lists them with a copy button for
// each reply and each block; panes match their rendered rows against them
// (panes.js). app-base.js creates it; protocol: GET
// /api/sessions/{id}/replies and the events socket's {"type":"replies"}
// pushes (a seq already known replaces that reply).
"use strict";

window.FwHubReplies = function (ctx) {
  const { el, icon, api, state, sessionById, isAgent } = ctx;

  const MAX_CACHED = 100;
  const cache = {}; // session id -> replies, newest first
  const loading = {}; // session id -> its fetch, while one runs

  // --- data --------------------------------------------------------------

  function remember(id, incoming) {
    const bySeq = new Map((cache[id] || []).map((r) => [r.seq, r]));
    for (const reply of incoming) bySeq.set(reply.seq, reply);
    cache[id] = [...bySeq.values()].sort((a, b) => b.seq - a.seq).slice(0, MAX_CACHED);
  }

  function load(id) {
    if (loading[id]) return loading[id];
    loading[id] = api("GET", `/api/sessions/${id}/replies?limit=${MAX_CACHED}`)
      .then((list) => {
        cache[id] = list.replies;
        if (ctx.onChange) ctx.onChange(id);
        return cache[id];
      })
      .catch((error) => {
        if (!cache[id]) cache[id] = [];
        throw error;
      })
      .finally(() => {
        delete loading[id];
      });
    return loading[id];
  }

  // Keep a session's replies at hand (fetched once; pushes keep it up).
  function ensure(id) {
    if (id && !cache[id] && !loading[id]) load(id).catch(() => {});
  }

  function onEvent(message) {
    const id = message.session_id;
    // Not fetched yet: the push is the newest only; a fetch gets them all.
    if (!cache[id]) {
      ensure(id);
      return;
    }
    remember(id, message.replies || []);
    if (ctx.onChange) ctx.onChange(id);
  }

  // Drop sessions that no longer exist.
  function forgetRemoved() {
    const live = new Set(state.sessions.map((s) => s.id));
    for (const id of Object.keys(cache)) {
      if (!live.has(id) && !loading[id]) delete cache[id];
    }
  }

  // The agent whose replies a session's actions take: itself, or the
  // agent a companion shell belongs to.
  function agentOf(session) {
    if (isAgent(session)) return session;
    const parent = session && session.companion_of ? sessionById(session.companion_of) : null;
    return isAgent(parent) ? parent : null;
  }

  // --- copying -----------------------------------------------------------

  async function copy(text, what) {
    const ok = await ctx.copyText(text);
    if (ok) {
      ctx.toast(el("span", {}, "Copied ", el("b", { textContent: `${text.length} chars` }), what ? ` ${what}` : ""), { timeout: 2200 });
    } else {
      ctx.toast("Copy failed: this browser refused the clipboard. Select the text in the Replies view instead.", { tone: "bad" });
    }
    return ok;
  }

  // Copy the newest reply of `id`'s agent (the attached session by
  // default). The clipboard wants the key press itself: with nothing
  // cached yet, the replies are fetched and copied once they are in.
  function copyLast(id = state.attached) {
    const agent = agentOf(sessionById(id));
    if (!agent) {
      ctx.toast("No agent session here: copy last reply takes a Claude Code or Codex session's.", { timeout: 2500 });
      return false;
    }
    const newest = (list) => (list && list.length ? list[0] : null);
    const known = newest(cache[agent.id]);
    if (known) {
      copy(known.text, `from ${agent.name}'s last reply`);
      return true;
    }
    load(agent.id).then(
      (list) => {
        const reply = newest(list);
        if (reply) copy(reply.text, `from ${agent.name}'s last reply`);
        else ctx.toast(`${agent.name} has not replied yet.`, { timeout: 2500 });
      },
      (error) => ctx.toast(`Could not read ${agent.name}'s replies: ${error.message}`, { tone: "bad" }),
    );
    return true;
  }

  // --- matching (panes.js) ----------------------------------------------

  // A text reduced to what a reply's match_key keeps: letters and digits.
  const reduce = (text) => String(text).replace(/[^\p{Alphabetic}\p{N}]/gu, "");
  // At least this much must agree (all of a shorter key).
  const MATCH_MIN = 12;

  // The newest cached reply of `id` whose rendering `rendered` is (its
  // rows reduced the same way agree with the reply's key as far as both
  // go), or null.
  function match(id, rendered) {
    const list = cache[id];
    if (!list || !rendered) return null;
    for (const reply of list) {
      const key = reply.match_key || "";
      const n = Math.min(key.length, rendered.length);
      if (n >= Math.min(MATCH_MIN, key.length) && n > 0 && key.slice(0, n) === rendered.slice(0, n)) return reply;
    }
    return null;
  }

  // --- the Replies view (in the Commands drawer) ---------------------------

  function when(timestamp) {
    if (!timestamp) return "";
    const at = new Date(timestamp);
    if (Number.isNaN(at.getTime())) return "";
    const seconds = Math.max(0, (Date.now() - at.getTime()) / 1000);
    if (seconds < 45) return "just now";
    if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
    if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
    return at.toLocaleDateString();
  }

  // Inline Markdown as nodes: `code` spans and **bold**; everything else
  // is text (nothing from the transcript is ever parsed as HTML).
  function inline(text) {
    const out = [];
    const pattern = /(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)|\*\*(?=\S)([^*\n]*?\S)\*\*/g;
    let last = 0;
    for (const m of text.matchAll(pattern)) {
      if (m.index > last) out.push(text.slice(last, m.index));
      out.push(m[1] ? el("code", { textContent: m[2] }) : el("strong", { textContent: m[3] }));
      last = m.index + m[0].length;
    }
    if (last < text.length) out.push(text.slice(last));
    return out;
  }

  function blockBody(block) {
    switch (block.kind) {
      case "code":
        return el("pre", { className: "reply-code" }, el("code", { textContent: block.code || "" }));
      case "table":
        return el("pre", { className: "reply-table", textContent: block.text });
      case "heading":
        return el("div", { className: "reply-heading" }, inline(block.text.replace(/^\s*#{1,6}\s*/, "").replace(/\s+#+\s*$/, "")));
      case "quote":
        return el("blockquote", { className: "reply-text" }, inline(block.text.replace(/^\s*> ?/gm, "")));
      default:
        return el("div", { className: "reply-text" }, inline(block.text));
    }
  }

  // A code block's Copy takes its code; any other block its Markdown.
  const blockCopyText = (block) => (block.kind === "code" ? block.code || "" : block.text);

  function renderBlock(block, n) {
    const what = block.kind === "code" ? `code${block.lang ? ` (${block.lang})` : ""}` : block.kind;
    return el(
      "div",
      { className: `reply-block ${block.kind}` },
      blockBody(block),
      el(
        "button",
        {
          type: "button",
          className: "icon reply-block-copy",
          title: `Copy this ${what} (block ${n})`,
          "aria-label": `Copy block ${n}: ${what}`,
          onclick: () => copy(blockCopyText(block), `(${what})`),
        },
        icon("copy"),
      ),
    );
  }

  function renderReply(reply, source, position) {
    const fallback = reply.source === "hook" || reply.source === "notify";
    return el(
      "li",
      { className: "cmd reply", dataset: { seq: String(reply.seq) } },
      el(
        "div",
        { className: "cmd-meta" },
        el("span", { className: "cmd-lang", textContent: position === 1 ? "last reply" : "reply" }),
        el("span", { className: "cmd-when", textContent: when(reply.timestamp), title: reply.timestamp || "" }),
        el("span", { className: "cmd-lines", textContent: `${reply.text.length} chars` }),
        fallback ? el("span", { className: "cmd-note", textContent: "from the hook", title: "Read from the agent's report; its transcript line replaces it once written" }) : null,
        reply.truncated ? el("span", { className: "cmd-note", textContent: "cut at 64 KiB", title: "Longer replies are kept up to 64 KiB" }) : null,
        el(
          "button",
          {
            type: "button",
            className: "slab ghost reply-copy",
            title: `Copy the whole reply's Markdown${position <= 9 ? ` (key ${position} in this view)` : ""}`,
            onclick: () => copy(reply.text, `from ${source.name}'s reply`),
          },
          icon("copy"),
          "Copy reply",
        ),
      ),
      el("div", { className: "reply-body" }, (reply.blocks || []).map((b, i) => renderBlock(b, i + 1))),
    );
  }

  // Fill the drawer's list with `source`'s replies, newest first; the
  // message to show instead when there are none.
  function render(list, source) {
    let empty = null;
    const replies = source ? cache[source.id] : null;
    if (!source) empty = "Replies appear here when a Claude Code or Codex session in this workspace answers.";
    else if (!replies) empty = "Reading the transcript…";
    else if (!replies.length) empty = `${source.name} has not replied yet.`;
    if (source && !replies) ensure(source.id);
    list.replaceChildren(
      ...(empty
        ? [el("li", { className: "cmd-empty", textContent: empty })]
        : replies.map((r, i) => renderReply(r, source, i + 1))),
    );
  }

  // The nth reply (1-based) of `source`, newest first.
  function nth(source, n) {
    const list = source ? cache[source.id] || [] : [];
    return list[n - 1] || null;
  }

  function copyNth(source, n) {
    const reply = nth(source, n);
    if (!reply) return false;
    copy(reply.text, `from ${source.name}'s reply`);
    return true;
  }

  const count = (id) => (cache[id] ? cache[id].length : 0);

  return {
    onEvent,
    load,
    ensure,
    copyLast,
    copy,
    match,
    reduce,
    render,
    copyNth,
    count,
    agentOf,
    forgetRemoved,
    cached: (id) => cache[id] || null,
  };
};

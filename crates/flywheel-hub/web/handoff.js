// flywheel hub: cross-agent handoff and review pairing. "Send to agent"
// composes a prompt from part of one session -- selected terminal text, a
// transcript message, a suggested block, its branch diff -- on the hub
// that owns it (POST /api/sessions/{from}/handoff/compose: the content
// the user picked under a "From <session> on <branch>:" header, capped
// with a notice), lets the user pick the agent and edit the prompt, and
// delivers it on the hub that owns the target (POST
// /api/sessions/{to}/handoff: bracketed paste, then Enter). Either hub
// may be a machine: calls naming a session go to its machine through
// this hub's proxy. "Ask for a review" launches (or asks again) a
// read-only reviewer of the other kind in the session's worktree (POST
// .../review/reviewer); the Review drawer shows the pair and the
// reviewer's findings (GET .../review/findings) with "Send findings to
// <author>" (POST .../review/findings/send). The sidebar's pair chips
// link author and reviewer. app-base.js creates it with its helpers.
"use strict";

window.FwHubHandoff = function (ctx) {
  const { $, el, icon, api, age, state, sessionById, workspaceById, isAgent } = ctx;
  const toast = (...args) => ctx.toast(...args);

  // How often an open findings panel re-reads the reviewer's answer.
  const FINDINGS_POLL_MS = 4000;
  // The target last sent to (picked first next time).
  const LAST_TARGET_KEY = "fw-hub.handoff-target";

  const dialog = $("#dlg-handoff");
  const text = $("#ho-text");
  const pick = $("#ho-to");
  const note = $("#ho-note");
  const sendButton = $("#ho-send");
  const ui = {
    mode: "handoff", // "handoff" | "findings"
    from: null, // source session id
    source: null, // the compose request's source
    composed: null, // the hub's answer
    findingsFor: null, // author id (findings mode)
    composedText: "", // the prompt as composed (edited when it differs)
    composingCross: null, // the cross-machine flag of the newest compose
    seq: 0,
    busy: false,
  };

  const machineOf = (session) => (session && session.machine) || "";
  const { bytes } = window.FwHubFmt;

  function sessionText(session) {
    const workspace = workspaceById(session.workspace_id);
    return [
      session.name,
      session.kind,
      workspace ? workspace.name : "",
      session.machine ? `@${session.machine}` : "",
    ].filter(Boolean).join(" · ");
  }

  // Agent sessions that can take a paste now.
  function targets(fromId) {
    return state.sessions.filter(
      (s) => isAgent(s) && s.id !== fromId && s.state === "running" && s.live && !s.fallback_command,
    );
  }

  function lastTarget() {
    try {
      return localStorage.getItem(LAST_TARGET_KEY) || "";
    } catch (_) {
      return "";
    }
  }

  // The likeliest target: the last one used, the source's review partner,
  // an agent of the other kind in its workspace, any agent in it.
  function defaultTarget(from, pool) {
    const has = (id) => id && pool.some((s) => s.id === id);
    if (has(lastTarget())) return lastTarget();
    if (from) {
      if (has(from.reviewer)) return from.reviewer;
      if (has(from.review_of)) return from.review_of;
      const here = pool.filter((s) => s.workspace_id === from.workspace_id);
      const other = here.find((s) => s.kind !== from.kind);
      if (other) return other.id;
      if (here.length) return here[0].id;
    }
    return pool.length ? pool[0].id : "";
  }

  function fillTargets(pool, chosen) {
    pick.replaceChildren(
      ...pool.map((s) => el("option", { value: s.id, textContent: sessionText(s), selected: s.id === chosen })),
    );
    pick.disabled = pool.length < 2 || ui.mode === "findings";
  }

  function setNote(message, tone) {
    note.textContent = message || "";
    note.dataset.tone = tone || "";
  }

  function renderSize() {
    const n = new TextEncoder().encode(text.value).length;
    const edited = ui.composedText && text.value !== ui.composedText;
    $("#ho-size").textContent = `${bytes(n)}${edited ? " · edited" : ""}`;
  }

  // --- the dialog ----------------------------------------------------------

  // Open "Send to agent" for `source` of session `from`. With `probe`,
  // the prompt is composed before the dialog opens: a source that may
  // have nothing (the last command) is a toast, not an empty dialog.
  async function open(from, source, probe) {
    const session = sessionById(from);
    if (!session) return;
    const pool = targets(from);
    if (!pool.length) {
      toast("No other agent session is running to send it to: start a Claude Code or Codex session first.", { tone: "bad" });
      return;
    }
    const chosen = defaultTarget(session, pool);
    let early = null;
    if (probe) {
      const to = sessionById(chosen);
      const cross = !!to && machineOf(to) !== machineOf(session);
      try {
        early = {
          composed: await api("POST", `/api/sessions/${session.id}/handoff/compose`, { source, cross_machine: cross }),
          cross,
        };
      } catch (error) {
        toast(error.message, { tone: "bad" });
        return;
      }
    }
    ui.mode = "handoff";
    ui.from = from;
    ui.source = source;
    ui.composed = null;
    ui.composedText = "";
    $("#ho-kicker").textContent = "send to agent";
    $("#ho-title").textContent = "Send to another agent";
    $("#ho-from").textContent = `From ${session.name}${machineOf(session) ? ` on ${session.machine}` : ""}`;
    fillTargets(pool, chosen);
    text.value = "";
    renderSize();
    setNote("Composing…");
    sendButton.disabled = true;
    sendButton.textContent = "Paste & send";
    if (!dialog.open) dialog.showModal();
    // The probe's prompt holds unless the target moved machines since.
    if (early && early.cross === crossNow()) {
      ui.seq++;
      ui.composingCross = early.cross;
      applyComposed(early.composed, early.cross);
    } else {
      compose();
    }
  }

  // Whether the picked target is on another machine than the source.
  function crossNow() {
    const from = sessionById(ui.from);
    const to = sessionById(pick.value);
    return !!from && !!to && machineOf(to) !== machineOf(from);
  }

  // The prompt shows from its first line: the provenance header.
  function showFromTop() {
    text.focus();
    text.setSelectionRange(0, 0);
    text.scrollTop = 0;
  }

  function applyComposed(composed, cross) {
    ui.composed = { ...composed, cross };
    // Never over what the user typed.
    if (!ui.composedText || text.value === ui.composedText) text.value = composed.prompt;
    ui.composedText = composed.prompt;
    renderSize();
    setNote(
      composed.truncated
        ? `Cut to ${bytes(composed.kept_bytes)} of ${bytes(composed.content_bytes)}: the prompt says so. Edit it freely; it is pasted, then Enter.`
        : `${composed.what.charAt(0).toUpperCase()}${composed.what.slice(1)}. Edit it freely; it is pasted into the agent, then Enter.`,
      composed.truncated ? "warn" : "",
    );
    sendButton.disabled = false;
    showFromTop();
  }

  async function compose() {
    const from = sessionById(ui.from);
    if (!from) return;
    const seq = ++ui.seq;
    const cross = crossNow();
    // Composed for one machine, never sent to another: Send waits.
    ui.composingCross = cross;
    sendButton.disabled = true;
    try {
      const composed = await api("POST", `/api/sessions/${from.id}/handoff/compose`, {
        source: ui.source,
        cross_machine: cross,
      });
      if (seq !== ui.seq || !dialog.open) return;
      applyComposed(composed, cross);
    } catch (error) {
      if (seq !== ui.seq) return;
      setNote(error.message, "bad");
    }
  }

  pick.addEventListener("change", () => {
    try {
      localStorage.setItem(LAST_TARGET_KEY, pick.value);
    } catch (_) {
      // Private mode: the pick lasts this dialog.
    }
    // The header names this machine only for a target elsewhere: a
    // compose (done or still running) for the other side is redone.
    if (ui.mode === "handoff" && crossNow() !== ui.composingCross) compose();
  });
  text.addEventListener("input", renderSize);
  text.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      send();
    }
  });
  sendButton.addEventListener("click", () => send());
  dialog.addEventListener("close", () => {
    ui.seq++;
    ui.busy = false;
  });

  async function send() {
    if (ui.busy) return;
    const prompt = text.value;
    if (!prompt.trim()) {
      setNote("The prompt is empty: nothing to send.", "bad");
      return;
    }
    const to = sessionById(pick.value);
    if (!to) {
      setNote("Pick the agent to send it to.", "bad");
      return;
    }
    if (ui.mode === "handoff" && (!ui.composed || ui.composed.cross !== crossNow())) {
      setNote("Composing for that agent's machine…");
      return;
    }
    ui.busy = true;
    sendButton.disabled = true;
    setNote(`Pasting into ${to.name}…`);
    try {
      if (ui.mode === "findings") {
        await api("POST", `/api/sessions/${ui.findingsFor}/review/findings/send`, { prompt });
      } else {
        const composed = ui.composed || {};
        await api("POST", `/api/sessions/${to.id}/handoff`, {
          prompt,
          from: composed.from || null,
          source: composed.source || null,
          truncated: !!composed.truncated,
        });
      }
      dialog.close("ok");
      toast(`Sent to ${to.name}.`);
    } catch (error) {
      setNote(error.message, "bad");
      sendButton.disabled = false;
    } finally {
      ui.busy = false;
    }
  }

  // --- entry points ------------------------------------------------------------

  // The focused terminal's selection, to the agent picked.
  function sendSelection(sessionId, selected) {
    const id = sessionId || state.attached;
    const textNow = selected !== undefined ? selected : ctx.selection();
    if (!id || !textNow || !textNow.trim()) {
      toast("Select text in the terminal first (drag over it), then send it.", { tone: "bad" });
      return false;
    }
    open(id, { kind: "selection", text: textNow, from: "terminal" });
    return true;
  }

  const sendLastCommand = (id) => open(id, { kind: "last_command" }, true);
  const sendCommand = (id, seq) => open(id, { kind: "command", seq });
  const sendDiff = (id, path, oldPath) => open(id, { kind: "diff", path: path || null, old_path: oldPath || null });
  // A transcript message, or the part of it selected in the viewer.
  function sendTranscript(id, index, selected) {
    if (selected && selected.trim()) open(id, { kind: "selection", text: selected, from: "transcript" });
    else open(id, { kind: "transcript", index });
  }

  // --- review pairing ----------------------------------------------------------

  // The author whose review `session` is part of.
  function authorOf(session) {
    if (!session) return null;
    if (session.review_of) return sessionById(session.review_of) || null;
    if (session.companion_of) return sessionById(session.companion_of) || session;
    return session;
  }

  // Authors with a review request in flight (one at a time each).
  const asking = new Set();

  async function askReview(id, kind) {
    const author = authorOf(sessionById(id));
    if (!author) return false;
    if (asking.has(author.id)) return true;
    asking.add(author.id);
    paint(author.id);
    try {
      const launched = await api("POST", `/api/sessions/${author.id}/review/reviewer`, kind ? { kind } : {});
      const reviewer = launched.reviewer;
      const how = launched.reused ? "Asked" : "Launched";
      toast(`${how} ${reviewer ? reviewer.name : "the reviewer"} (${launched.kind}, read-only: ${launched.flags.join(" ")}) on ${launched.branch || "HEAD"} vs ${launched.base}.${launched.fallback ? ` ${launched.fallback}.` : ""}${launched.diff_truncated ? " The diff in its prompt is cut; it reads the rest itself." : ""}`);
      const panel = panels.get(author.id);
      if (panel) panel.data = null;
      if (launched.replaced) toast(`${launched.replaced.name} (${launched.replaced.kind}) is replaced by ${reviewer ? reviewer.name : "the new reviewer"}: it is closed.`);
      await ctx.refresh();
      loadFindings(author.id);
    } catch (error) {
      toast(error.message, { tone: "bad" });
    } finally {
      asking.delete(author.id);
      paint(author.id);
    }
    return true;
  }

  // Findings panels, one per author (kept across drawer re-renders).
  const panels = new Map(); // author id -> {node, data, error, loading, at, timer}

  async function loadFindings(authorId) {
    const panel = panels.get(authorId);
    if (!panel || panel.loading) return;
    panel.loading = true;
    try {
      panel.data = await api("GET", `/api/sessions/${authorId}/review/findings`);
      panel.error = "";
    } catch (error) {
      panel.error = error.status === 409 ? "" : error.message;
      panel.data = null;
    } finally {
      panel.loading = false;
      panel.at = Date.now();
    }
    paint(authorId);
  }

  function blockCard(block, author) {
    return el(
      "li",
      { className: `ho-block${block.shell ? " shell" : ""}` },
      el("div", { className: "ho-block-head" },
        el("span", { className: "cmd-lang", textContent: block.lang || (block.shell ? "shell" : "text") }),
        el("span", { className: "ho-block-actions" },
          el("button", {
            type: "button",
            className: "text-btn",
            textContent: "Copy",
            onclick: async () => {
              const ok = await ctx.copyText(block.command);
              toast(ok ? "Copied." : "Copy failed: select the text instead.", { tone: ok ? "" : "bad" });
            },
          }),
          author
            ? el("button", {
              type: "button",
              className: "text-btn",
              textContent: `To ${author.name}…`,
              title: `Send this block to ${author.name} (editable first)`,
              onclick: () => openFindings(author.id, block),
            })
            : null,
        ),
      ),
      el("pre", { className: "cmd-code" }, el("code", { textContent: block.text })),
    );
  }

  // The reply's lines outside code fences and its fenced blocks, in
  // order (the fences as commands.rs reads them; blocks with nothing in
  // them are dropped, as the hub drops them).
  function splitReply(reply) {
    const parts = [];
    let prose = [];
    let open = null; // {ch, len, indent, lines}
    const flush = () => {
      if (prose.join("\n").trim()) parts.push({ prose: prose.join("\n").replace(/^\n+|\n+$/g, "") });
      prose = [];
    };
    for (const raw of reply.split("\n")) {
      const line = raw.replace(/\r$/, "");
      if (!open) {
        const m = /^( *)(`{3,}|~{3,})(.*)$/.exec(line);
        if (m && !(m[2][0] === "`" && m[3].includes("`"))) {
          flush();
          open = { ch: m[2][0], len: m[2].length, indent: m[1].length, lines: [] };
        } else {
          prose.push(raw);
        }
        continue;
      }
      const rest = line.replace(/^ +/, "");
      let n = 0;
      while (rest[n] === open.ch) n++;
      if (n >= open.len && !rest.slice(n).trim()) {
        parts.push({ text: open.lines.join("\n") });
        open = null;
      } else {
        const spaces = raw.length - raw.replace(/^ +/, "").length;
        open.lines.push(raw.slice(Math.min(spaces, open.indent)));
      }
    }
    if (open) {
      if (open.lines.length && open.lines[open.lines.length - 1] === "") open.lines.pop();
      parts.push({ text: open.lines.join("\n") });
    }
    flush();
    return parts.filter((p) => p.prose !== undefined || p.text.trim());
  }

  // The reply as prose with its code blocks as cards in place (each block
  // once); the raw reply and the cards below it when the split does not
  // match the hub's blocks.
  function replyView(data, author) {
    const parts = splitReply(data.reply);
    const blocks = parts.filter((p) => p.prose === undefined);
    const matches = blocks.length === data.blocks.length && blocks.every((b, i) => b.text === data.blocks[i].text);
    if (!matches) {
      return el("div", { className: "ho-reply-wrap" },
        el("pre", { className: "ho-reply" }, el("code", { textContent: data.reply })),
        data.blocks.length ? el("ul", { className: "ho-blocks" }, ...data.blocks.map((b) => blockCard(b, author))) : null);
    }
    let next = 0;
    return el("div", { className: "ho-reply rich" },
      ...parts.map((part) => (part.prose !== undefined
        ? el("p", { className: "ho-prose", textContent: part.prose })
        : el("ul", { className: "ho-blocks" }, blockCard(data.blocks[next++], author)))));
  }

  function paint(authorId) {
    const panel = panels.get(authorId);
    if (!panel) return;
    const author = sessionById(authorId);
    const reviewer = author && author.reviewer ? sessionById(author.reviewer) : null;
    const data = panel.data;
    const kindPick = el(
      "select",
      { className: "ho-kind", "aria-label": "Reviewer kind", title: "Which agent reviews (default: the other kind)" },
      el("option", { value: "", textContent: "other kind" }),
      el("option", { value: "claude", textContent: "claude" }),
      el("option", { value: "codex", textContent: "codex" }),
    );
    // "Ask again" asks the paired reviewer unless another kind is picked.
    kindPick.value = panel.kind || (reviewer ? reviewer.kind : "");
    kindPick.addEventListener("change", () => {
      panel.kind = kindPick.value;
    });
    // Its pushed status, else the one the findings read.
    const status = reviewer
      ? ctx.agentStatus(reviewer) || (data && data.reviewer && data.reviewer.status ? { status: data.reviewer.status } : null)
      : null;
    const pending = asking.has(authorId);
    const head = el(
      "div",
      { className: "ho-review-head" },
      el("span", { className: "ho-review-label" }, icon("branch"), "Peer review"),
      reviewer
        ? el("button", {
          type: "button",
          className: "pair-chip",
          title: `Open ${reviewer.name}`,
          onclick: () => ctx.openSession(reviewer.id),
        },
        el("span", { className: `dot ${status ? status.status : "unknown"}` }),
        `${reviewer.name} · ${reviewer.kind}${status && status.status ? ` · ${status.status}` : ""}`)
        : el("span", { className: "ho-review-hint", textContent: "Another agent reads the diff, read-only, and reports back." }),
      el("span", { className: "ho-review-actions" },
        kindPick,
        el("button", {
          type: "button",
          className: "slab ghost",
          textContent: pending ? "Asking…" : reviewer ? "Ask again" : "Ask for a review",
          disabled: pending,
          title: "Launch (or ask again) a reviewer agent in this worktree, read-only, with the branch diff",
          onclick: () => askReview(authorId, kindPick.value || null),
        }),
      ),
    );
    const parts = [head];
    if (panel.error) parts.push(el("p", { className: "ho-review-note bad", textContent: panel.error }));
    if (reviewer && data) {
      if (data.reply) {
        const working = status && status.status === "working";
        const when = data.timestamp ? age(data.timestamp) : "";
        parts.push(el("p", {
          className: "ho-review-meta",
          textContent: [
            when ? `Answered ${when} ago` : "Answered",
            working ? `${reviewer.name} is still working: this is its reply so far` : "",
          ].filter(Boolean).join(" · "),
          title: data.timestamp || "",
        }));
        const canSend = !!(author && isAgent(author) && author.live && author.state === "running" && !author.fallback_command);
        parts.push(replyView(data, canSend ? author : null));
        parts.push(el("div", { className: "ho-review-foot" },
          el("button", {
            type: "button",
            className: "slab",
            disabled: !canSend,
            title: !author || !isAgent(author)
              ? "The author is not an agent session"
              : author.fallback_command
                ? `${author.name} came back as a plain shell after a restart: kill and relaunch it to start ${author.kind} again, then send`
                : canSend ? `Paste the findings into ${author.name} (editable first), then Enter` : `${author.name} is not running`,
            onclick: () => openFindings(authorId),
          }, icon("send"), `Send findings to ${author ? author.name : "the author"}…`),
        ));
      } else {
        parts.push(el("p", { className: "ho-review-note", textContent: data.reviewer.running ? `${reviewer.name} is reviewing; its answer shows here.` : `${reviewer.name} has written no answer.` }));
      }
    }
    panel.node.replaceChildren(...parts);
  }

  // The panel for the Review drawer of `session` (null: nothing to pair).
  function reviewPanel(session) {
    const author = authorOf(session);
    if (!author) return null;
    let panel = panels.get(author.id);
    if (!panel) {
      panel = { node: el("section", { className: "ho-review", "aria-label": "Peer review" }), data: null, error: "", loading: false, at: 0, timer: null, kind: "" };
      panels.set(author.id, panel);
    }
    paint(author.id);
    const stale = Date.now() - panel.at > FINDINGS_POLL_MS;
    if (author.reviewer && stale) loadFindings(author.id);
    clearInterval(panel.timer);
    panel.timer = setInterval(() => {
      if (!panel.node.isConnected) {
        clearInterval(panel.timer);
        return;
      }
      const current = sessionById(author.id);
      if (current && current.reviewer && !document.hidden) loadFindings(author.id);
    }, FINDINGS_POLL_MS);
    return panel.node;
  }

  // "Send findings": the composed findings (or one block of them),
  // editable, into the author.
  async function openFindings(authorId, block) {
    const author = sessionById(authorId);
    if (!author) return;
    let findings;
    try {
      findings = await api("GET", `/api/sessions/${authorId}/review/findings`);
    } catch (error) {
      toast(error.message, { tone: "bad" });
      return;
    }
    if (!findings.prompt) {
      toast(`${findings.reviewer.name} has not answered yet.`, { tone: "bad" });
      return;
    }
    ui.mode = "findings";
    ui.findingsFor = authorId;
    ui.composed = null;
    let prompt = findings.prompt;
    if (block) {
      const header = prompt.split("\n")[0];
      const fence = "`".repeat(Math.max(3, ...[...block.text.matchAll(/`+/g)].map((m) => m[0].length + 1)));
      prompt = `${header}\n\n${fence}${block.lang || ""}\n${block.text}\n${fence}\n`;
    }
    ui.composedText = prompt;
    $("#ho-kicker").textContent = "review findings";
    $("#ho-title").textContent = `Send findings to ${author.name}`;
    $("#ho-from").textContent = `From ${findings.reviewer.name} (${findings.reviewer.kind}, read-only)`;
    fillTargets([author], author.id);
    text.value = prompt;
    renderSize();
    setNote(findings.truncated ? "Cut to the size cap: the prompt says so." : "Edit it freely; it is pasted into the author, then Enter.", findings.truncated ? "warn" : "");
    sendButton.disabled = false;
    sendButton.textContent = "Paste & send";
    if (!dialog.open) dialog.showModal();
    showFromTop();
  }

  // --- the sidebar ---------------------------------------------------------------

  // A row's pair chip: "reviews A" on a reviewer, "reviewed by R" on its
  // author; a click opens the other.
  function pairChip(session) {
    const peerId = session.review_of || session.reviewer;
    if (!peerId) return null;
    const peer = sessionById(peerId);
    if (!peer) return null;
    const reviews = !!session.review_of;
    return el(
      "button",
      {
        type: "button",
        className: `pair-chip${reviews ? " reviews" : ""}`,
        title: reviews ? `Reviews ${peer.name}'s work (read-only): open it` : `Reviewed by ${peer.name}: open it`,
        onclick: (event) => {
          event.stopPropagation();
          ctx.openSession(peer.id);
        },
      },
      icon("branch"),
      el("span", { textContent: reviews ? `reviews ${peer.name}` : `reviewed by ${peer.name}` }),
    );
  }

  // --- palette ---------------------------------------------------------------------

  function paletteItems(cmd) {
    const session = sessionById(state.attached);
    if (!session) return;
    const key = (id) => ctx.keyOf(id);
    const hasOthers = targets(session.id).length > 0;
    if (hasOthers) {
      cmd("Send selection to another agent…", [key("handoff.send"), "the terminal text selected, with where it is from"].filter(Boolean).join(": "), () => sendSelection(session.id), "send");
      if (isAgent(session)) cmd(`Send ${session.name}'s last command to an agent…`, "its newest suggested shell command", () => sendLastCommand(session.id), "send");
      if (session.git || session.worktree) cmd(`Send ${session.name}'s branch diff to an agent…`, "git diff <base>...HEAD, capped", () => sendDiff(session.id), "send");
    }
    if (session.git || session.worktree) {
      cmd(`Ask for a review of ${session.name}`, [key("review.ask"), "a read-only reviewer of the other kind, in its worktree"].filter(Boolean).join(": "), () => askReview(session.id), "branch");
    }
  }

  return {
    open,
    sendSelection,
    sendLastCommand,
    sendCommand,
    sendDiff,
    sendTranscript,
    askReview,
    reviewPanel,
    pairChip,
    paletteItems,
    targets,
    authorOf,
  };
};

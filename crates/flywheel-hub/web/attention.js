// flywheel hub: the attention list. Every agent that needs you, across all
// workspaces, heading the sidebar's Everything list (and the phone's
// start screen):
// blocked ones first (longest waiting first), then handoffs agents
// proposed (each its own row, oldest first, whatever the agent's status
// and whoever watches it; kept on the session until started or
// dismissed), then sessions agents asked to start (the same, until
// started or declined), then sessions agents asked to end or replace
// (a session they do not own, or a replacement their start policy asks
// about; the same, until approved or declined), then agents whose turn
// ended asking you something (telemetry's `asking`: a reply, a decision,
// or a command only you can run; answered in the row: the reply goes
// through POST /api/broadcast, typed in now or queued), then prompt queues that
// stalled (queue.js: the agent exited, a paste was refused), then fan-outs whose graders
// finished since this browser looked (fanout.js; their candidates' own
// finished turns are not listed), then finished turns this browser has
// not looked at (newest first), then flywheel tasks that failed,
// stranded, stopped, or wait on a gate (flywheel.js; a folder with many
// is one row, and alerts marked seen in its panel drop out). Tap one to open it (a flywheel row opens its folder's
// Flywheel panel, a fan-out row its board).
// Holds (GET /api/holds) of one kind and cause, two or more, are one
// group row answered at once (POST /api/holds/answer); tapped (always on
// the start screen) it lists its members, each one can be left out or
// answered its own way. Their own rows step aside while it shows.
"use strict";

window.FwHubAttention = function (ctx) {
  const { $, el, morph, icon, age, state, workspaceById, agentStatus, openSession, flywheel, fanout } = ctx;

  // What an asking session's telemetry says it asks (else null).
  function askOf(session, agent) {
    const snap = ctx.telemetryOf ? ctx.telemetryOf(session.id) : null;
    return agent.state === "asking" && snap && snap.ask ? snap.ask : null;
  }

  // What a row says it needs, its kind first, the rest quieter:
  // "permission: Bash  rm -rf build", "decide: which parser API?",
  // "done: fix the failing test".
  function needLine(session, agent, ask) {
    const snap = ctx.telemetryOf ? ctx.telemetryOf(session.id) : null;
    const said = (kind, rest) => [`${kind}: `, el("span", { className: "muted", textContent: rest || "" })];
    if (agent.status === "blocked") {
      if (snap && snap.blocked_tool) {
        const call = snap.tool && snap.tool.name === snap.blocked_tool ? snap.tool.summary : "";
        return said("permission", call ? `${snap.blocked_tool} ${call}` : snap.blocked_tool);
      }
      return agent.detail || "waiting on you";
    }
    if (ask) return said(ASK_WORD[ask.kind] || "asks", ask.text);
    if (agent.state === "failed") return said("failed", snap && snap.last_error);
    return said("done", (snap && snap.turn_title) || "finished its turn");
  }
  const ASK_WORD = { answer: "asks", decide: "decide", run: "run this", handoff: "hand off" };
  // Lines of a pending handoff's prompt its row shows.
  const HANDOFF_PREVIEW = 4;
  // Handoff prompts being edited, by session.
  const handoffs = new Map();
  // Replies being typed, by session: a re-render keeps them.
  const drafts = new Map();

  // Agents that need this browser, most urgent first. A fan-out
  // candidate's finished turn is not one: the hub grades it, and its
  // fan-out's row says when the verdicts are in.
  function entries() {
    const out = [];
    const graded = fanout ? fanout.candidateSessions() : new Set();
    for (const session of state.sessions) {
      const agent = agentStatus(session);
      if (!agent || (agent.status !== "blocked" && agent.status !== "done")) continue;
      if (agent.status === "done" && graded.has(session.id)) continue;
      // A handoff accepted: its successor carries the work on, so the
      // turn that ended asking for it needs nobody.
      const snap = agent.status === "done" && ctx.telemetryOf ? ctx.telemetryOf(session.id) : null;
      if (snap && snap.continued_by) continue;
      const ask = agent.status === "done" ? askOf(session, agent) : null;
      out.push({ session, agent, ask, level: agent.status === "blocked" ? 0 : ask ? 1 : 2 });
    }
    const time = (e) => (e.ask ? e.ask.since : Date.parse(e.agent.since)) || 0;
    // Blocked and asking: longest waiting first; finished: newest first.
    out.sort((a, b) => a.level - b.level || (a.level < 2 ? time(a) - time(b) : time(b) - time(a)));
    return out;
  }

  // Every pending handoff, oldest first (a new one goes last: the rows
  // above it never move).
  function handoffEntries() {
    const out = [];
    for (const session of state.sessions) {
      for (const handoff of session.handoffs || []) out.push({ session, handoff });
    }
    out.sort((a, b) => Date.parse(a.handoff.created_at) - Date.parse(b.handoff.created_at) || (a.handoff.id < b.handoff.id ? -1 : 1));
    return out;
  }

  // Every agent start waiting on the operator, oldest first (like
  // handoffs: a new one goes last).
  function startEntries() {
    const out = [];
    for (const session of state.sessions) {
      for (const start of session.start_requests || []) out.push({ session, start });
    }
    out.sort((a, b) => Date.parse(a.start.created_at) - Date.parse(b.start.created_at) || (a.start.id < b.start.id ? -1 : 1));
    return out;
  }

  // Every agent end or replacement waiting on the operator, oldest first.
  function endEntries() {
    const out = [];
    for (const session of state.sessions) {
      for (const end of session.end_requests || []) out.push({ session, end });
    }
    out.sort((a, b) => Date.parse(a.end.created_at) - Date.parse(b.end.created_at) || (a.end.id < b.end.id ? -1 : 1));
    return out;
  }

  // --- hold groups -------------------------------------------------------

  const KIND_WORD = { needs_input: "needs input", awaiting_approval: "approval", permission: "permission", gate: "gate" };
  // Groups of two or more holds, as the hub last listed them.
  let holdGroups = [];
  // Stuck leases (holds `lease:...`): a row each, whatever their count.
  let leaseHolds = [];
  let holdsSig = "";
  let holdsAt = 0;
  let holdsBusy = false;
  // Members left out of their group's answer, and members answered their
  // own way, by hold id; a group's reply being typed, by group key.
  const leftOut = new Set();
  const ownAnswer = new Map();
  const groupDrafts = new Map();
  const groupKey = (g) => `${g.kind}\u0000${g.cause}`;

  // Re-read the holds when what this browser sees waiting changes (and at
  // most every 20 s otherwise); repaint only when the groups changed.
  function refreshHolds(sig) {
    if (holdsBusy || (sig === holdsSig && Date.now() - holdsAt < 20000)) return;
    holdsBusy = true;
    holdsSig = sig;
    ctx
      .api("GET", "/api/holds")
      .then((listed) => {
        const isLease = (h) => /^lease:/.test(h.id);
        const leases = (listed.groups || []).flatMap((g) => (g.holds || []).filter(isLease));
        const next = (listed.groups || [])
          .map((g) => ({ ...g, holds: (g.holds || []).filter((h) => !isLease(h)) }))
          .filter((g) => g.holds.length > 1);
        const changed = JSON.stringify(next) !== JSON.stringify(holdGroups) || JSON.stringify(leases) !== JSON.stringify(leaseHolds);
        holdGroups = next;
        leaseHolds = leases;
        if (!changed) return;
        if (ctx.rerender) ctx.rerender();
        else render();
      })
      .catch(() => {})
      .finally(() => {
        holdsAt = Date.now();
        holdsBusy = false;
      });
  }

  // Hold ids (and permission-blocked sessions) a group stands for.
  function grouped() {
    const ids = new Set();
    const blocked = new Set();
    for (const g of holdGroups) {
      for (const h of g.holds) {
        ids.add(h.id);
        if (h.kind === "permission" && h.session) blocked.add(h.session);
      }
    }
    return { ids, blocked };
  }

  async function answerGroup(g, answer, node) {
    const text = String(answer || "").trim();
    if (!text) return;
    const overrides = {};
    for (const h of g.holds) {
      if (leftOut.has(h.id)) overrides[h.id] = "skip";
      else if (ownAnswer.get(h.id)) overrides[h.id] = ownAnswer.get(h.id);
    }
    if (g.holds.every((h) => overrides[h.id] === "skip")) {
      ctx.toast("Every hold in the group is left out");
      return;
    }
    if (node) node.classList.add("busy");
    try {
      const done = await ctx.api("POST", "/api/holds/answer", { holds: g.holds.map((h) => h.id), answer: text, overrides });
      const results = done.results || [];
      const failed = results.filter((r) => !r.ok);
      const answered = results.filter((r) => r.ok && r.said !== "skipped").length;
      for (const h of g.holds) {
        leftOut.delete(h.id);
        ownAnswer.delete(h.id);
      }
      groupDrafts.delete(groupKey(g));
      ctx.toast(
        failed.length ? `Answered ${answered}; ${failed.length} not: ${failed[0].error}` : `Answered ${answered} as one decision`,
        { tone: failed.length ? "bad" : "" },
      );
    } catch (error) {
      ctx.toast(`Not answered: ${error.message}`, { tone: "bad" });
    } finally {
      if (node) node.classList.remove("busy");
      holdsAt = 0;
      refreshHolds(holdsSig);
    }
  }

  // A member's own answer: approvals and gates pick, asks type.
  function memberChoice(g, h) {
    if (g.kind === "permission") return null;
    if (g.kind === "needs_input") {
      return el("input", {
        type: "text",
        className: "attn-reply-input attn-own",
        placeholder: "own reply",
        "aria-label": `Own reply to ${h.session_name || h.id}`,
        value: ownAnswer.get(h.id) || "",
        oninput: (event) => (event.target.value.trim() ? ownAnswer.set(h.id, event.target.value) : ownAnswer.delete(h.id)),
      });
    }
    const no = g.kind === "gate" ? "deny" : "decline";
    return el(
      "select",
      {
        className: "attn-own",
        "aria-label": `Answer for ${h.session_name || h.id}`,
        onchange: (event) => (event.target.value ? ownAnswer.set(h.id, event.target.value) : ownAnswer.delete(h.id)),
      },
      ["", "approve", no].map((v) => el("option", { value: v, selected: (ownAnswer.get(h.id) || "") === v, textContent: v || "as group" })),
    );
  }

  // What tells a member from the rest: its session (when they differ)
  // and its question without the asker's name or the start's kind
  // ("\"w1\": task 1").
  function memberText(g, h) {
    const sessions = new Set(g.holds.map((m) => m.session || m.folder || ""));
    const name = sessions.size > 1 ? h.session_name || (h.folder || "").split("/").pop() : null;
    let text = h.question || "";
    if (h.session_name && text.startsWith(`${h.session_name} asks to `)) text = text.slice(`${h.session_name} asks to `.length);
    // A start's kind is the same down the group: what tells them apart is
    // the name and the task.
    text = text.replace(/^start an? \S+ session /, "");
    return { name, text };
  }


  // Groups opened in the sidebar (the start screen shows every group open).
  const groupOpen = new Set();

  // One group: a line like the others (its count, its cause, its kind,
  // since when; who it is under it), then, once tapped (always on the
  // start screen), its members and the answer for them all.
  function groupRow(g, full) {
    const key = groupKey(g);
    const open = full || groupOpen.has(key);
    const oldest = Math.min(...g.holds.map((h) => h.since || Date.now()));
    const recommended = g.holds.map((h) => h.recommendation).filter(Boolean);
    const who = g.holds.map((h) => {
      const m = memberText(g, h);
      return m.name || (m.text.match(/"([^"]+)"/) || [])[1] || h.session_name || h.id;
    });
    const detail = recommended.length && recommended.every((r) => r === recommended[0]) ? `recommended: ${recommended[0]}` : who.join(", ");
    const head = el(
      "button",
      {
        type: "button",
        className: `attn-row group ${g.kind}`,
        "aria-expanded": full ? undefined : String(open),
        title: full ? g.cause : open ? "Fold the group" : `${g.holds.length} like holds, answered as one: open`,
        onclick: () => {
          if (full) return;
          if (groupOpen.has(key)) groupOpen.delete(key);
          else groupOpen.add(key);
          if (ctx.rerender) ctx.rerender();
          else render();
        },
      },
      el("span", { className: "glyph count", textContent: String(g.holds.length) }),
      el(
        "span",
        { className: "attn-text" },
        el("span", { className: "attn-line" }, el("b", { className: "attn-name", textContent: g.cause, title: g.cause })),
        el("span", { className: "attn-detail", textContent: detail, title: detail }),
      ),
      el(
        "span",
        { className: "attn-meta" },
        el("span", { className: `attn-status group ${g.kind}`, textContent: KIND_WORD[g.kind] || g.kind }),
        el("span", { className: "attn-age", textContent: age(new Date(oldest).toISOString()) }),
      ),
    );
    if (!open) return el("li", { dataset: { id: `group:${key}` }, className: "attn-group" }, head);
    const members = el(
      "ul",
      { className: "attn-members" },
      g.holds.map((h) =>
        el(
          "li",
          { dataset: { id: h.id }, className: leftOut.has(h.id) ? "out" : "" },
          el(
            "label",
            { className: "attn-member", title: h.question },
            el("input", {
              type: "checkbox",
              checked: !leftOut.has(h.id),
              "aria-label": `Include ${h.session_name || h.id}`,
              onchange: (event) => {
                if (event.target.checked) leftOut.delete(h.id);
                else leftOut.add(h.id);
                event.target.closest("li").classList.toggle("out", !event.target.checked);
              },
            }),
            memberText(g, h).name ? el("b", { textContent: memberText(g, h).name }) : null,
            el("span", { className: "muted", textContent: memberText(g, h).text }),
          ),
          memberChoice(g, h),
        ),
      ),
    );
    const button = (label, answer, ghost) =>
      el("button", { type: "button", className: `slab${ghost ? " ghost" : ""} attn-group-answer`, textContent: label, onclick: (event) => answerGroup(g, answer, event.target.closest(".attn-group")) });
    let actions;
    if (g.kind === "awaiting_approval" && g.holds.every((h) => (h.answers || []).includes("approve"))) {
      actions = el("div", { className: "attn-handoff-actions" }, button("Decline all", "decline", true), button("Approve all", "approve"));
    } else if (g.kind === "gate") {
      actions = el("div", { className: "attn-handoff-actions" }, button("Deny all", "deny", true), button("Approve all", "approve"));
    } else if (g.kind === "permission") {
      actions = el("div", { className: "attn-handoff-actions" }, button("Yes to all (once)", "yes"));
    } else {
      const field = el("input", {
        type: "text",
        className: "attn-reply-input",
        placeholder: "Reply to all",
        "aria-label": `Reply to all ${g.holds.length}`,
        value: groupDrafts.get(key) || "",
        oninput: (event) => groupDrafts.set(key, event.target.value),
      });
      const shared = (g.holds[0].answers || []).filter((a) => g.holds.every((h) => (h.answers || []).includes(a)));
      const form = el(
        "form",
        {
          className: "attn-reply",
          onsubmit: (event) => {
            event.preventDefault();
            answerGroup(g, field.value, form);
          },
        },
        field,
        el("button", { type: "submit", className: "slab attn-send", textContent: "Send all" }),
      );
      actions = el(
        "div",
        { className: "attn-group-reply" },
        shared.length ? el("div", { className: "attn-options" }, shared.map((a) => el("button", { type: "button", className: "chip attn-option", textContent: a, title: `Reply "${a}" to all`, onclick: () => answerGroup(g, a, null) }))) : null,
        form,
      );
    }
    return el("li", { dataset: { id: `group:${key}` }, className: "attn-group open" }, head, el("div", { className: "attn-ask attn-pending" }, members, actions));
  }

  // A worktree session's branch (the badge cache's, else the one it was
  // cut on): which of an agent's parallel tasks this is.
  const branchOf = (session) =>
    session.worktree ? (session.git && session.git.branch) || session.worktree.branch : null;

  async function reply(session, text, form) {
    const prompt = String(text || "").trim();
    if (!prompt) return;
    if (form) form.classList.add("busy");
    try {
      const done = await ctx.api("POST", "/api/broadcast", { targets: [session.id], text: prompt });
      const result = (done.results || [])[0] || {};
      if (result.result === "refused") {
        ctx.toast(`Not sent to ${session.name}: ${result.why || "refused"}`, { tone: "bad" });
        return;
      }
      drafts.delete(session.id);
      ctx.toast(result.result === "queued" ? `Queued for ${session.name}` : `Sent to ${session.name}`);
    } catch (error) {
      ctx.toast(`Not sent to ${session.name}: ${error.message}`, { tone: "bad" });
    } finally {
      if (form) form.classList.remove("busy");
    }
  }

  // Start a handoff: a new session in the same directory continues the
  // work; it opens. `pending`: the kept handoff (its prompt and name
  // unless `edits` change them); none: a handoff from before they were
  // kept, from `edits.prompt`.
  async function startSuccessor(session, pending, edits, button) {
    if (button) button.disabled = true;
    try {
      const body = {};
      if (pending) body.handoff = pending.id;
      if (edits && edits.prompt != null && (!pending || edits.prompt !== pending.prompt)) body.prompt = edits.prompt;
      if (edits && edits.name && (!pending || edits.name !== (pending.name || ""))) body.name = edits.name;
      const next = await ctx.api("POST", `/api/sessions/${encodeURIComponent(session.id)}/continue`, body);
      handoffs.delete(session.id);
      ctx.toast(`Started ${next.name}, continuing ${session.name}`);
      // The new session is listed before it is opened.
      if (ctx.refresh) await ctx.refresh();
      openSession(next.id);
      return true;
    } catch (error) {
      ctx.toast(`Not started: ${error.message}`, { tone: "bad" });
      return false;
    } finally {
      if (button) button.disabled = false;
    }
  }

  async function dismiss(session, pending, button) {
    const sure = ctx.confirm
      ? await ctx.confirm({
          kicker: "handoff",
          title: `Dismiss ${session.name}'s handoff?`,
          body: `The proposed session${pending.name ? ` "${pending.name}"` : ""} is not started and its prompt is dropped.`,
          ok: "Dismiss",
        })
      : true;
    if (!sure) return;
    if (button) button.disabled = true;
    try {
      await ctx.api("DELETE", `/api/sessions/${encodeURIComponent(session.id)}/handoffs/${encodeURIComponent(pending.id)}`);
      if (ctx.refresh) await ctx.refresh();
    } catch (error) {
      ctx.toast(`Not dismissed: ${error.message}`, { tone: "bad" });
    } finally {
      if (button) button.disabled = false;
    }
  }

  // The review sheet (shared by handoffs and agent starts), worded for
  // one of them.
  function sheet(kicker, title, startTitle) {
    const dialog = $("#dlg-successor");
    if (!dialog) return null;
    dialog.querySelector(".sheet-kicker").textContent = kicker;
    $("#sc-title").textContent = title;
    $("#sc-start").title = startTitle;
    return dialog;
  }

  // Review a handoff in a large editor: its name and whole prompt, then
  // Start.
  function review(session, pending) {
    const dialog = sheet("handoff", "Start the new session", "Start it here with this prompt; it continues the old session's work");
    if (!dialog) return;
    $("#sc-from").textContent = `${session.name} proposes${pending.reason ? `: ${pending.reason}` : " a new session"}`;
    const name = $("#sc-name");
    const text = $("#sc-text");
    name.value = pending.name || "";
    name.placeholder = `${session.name} 2`;
    text.value = pending.prompt;
    const size = () => {
      $("#sc-size").textContent = `${text.value.length.toLocaleString()} chars`;
    };
    size();
    text.oninput = size;
    const start = $("#sc-start");
    start.onclick = async () => {
      const prompt = text.value;
      if (!prompt.trim()) {
        ctx.toast("The prompt is empty", { tone: "bad" });
        return;
      }
      const started = await startSuccessor(session, pending, { prompt, name: name.value.trim() }, start);
      if (started) dialog.close("ok");
    };
    dialog.showModal();
    text.focus();
    text.setSelectionRange(0, 0);
    text.scrollTop = 0;
  }

  // Start a session an agent asked for (limits are checked again); the
  // agent is told. `edits`: a changed prompt or name from Review.
  async function approveStart(session, pending, edits, button) {
    if (button) button.disabled = true;
    try {
      const body = {};
      if (edits && edits.prompt != null && edits.prompt !== (pending.prompt || "")) body.prompt = edits.prompt;
      if (edits && edits.name && edits.name !== (pending.name || "")) body.name = edits.name;
      const next = await ctx.api("POST", `/api/sessions/${encodeURIComponent(session.id)}/starts/${encodeURIComponent(pending.id)}`, body);
      ctx.toast(`Started ${next.name} for ${session.name}`);
      if (ctx.refresh) await ctx.refresh();
      return true;
    } catch (error) {
      ctx.toast(`Not started: ${error.message}`, { tone: "bad" });
      return false;
    } finally {
      if (button) button.disabled = false;
    }
  }

  async function declineStart(session, pending, button) {
    const sure = ctx.confirm
      ? await ctx.confirm({
          kicker: "start request",
          title: `Decline ${session.name}'s request?`,
          body: `The session${pending.name ? ` "${pending.name}"` : ""} is not started; ${session.name} is told.`,
          ok: "Decline",
        })
      : true;
    if (!sure) return;
    if (button) button.disabled = true;
    try {
      await ctx.api("DELETE", `/api/sessions/${encodeURIComponent(session.id)}/starts/${encodeURIComponent(pending.id)}`);
      if (ctx.refresh) await ctx.refresh();
    } catch (error) {
      ctx.toast(`Not declined: ${error.message}`, { tone: "bad" });
    } finally {
      if (button) button.disabled = false;
    }
  }

  // Where a requested session runs, in a few words.
  function startWhere(session, pending) {
    const dir = (path) => String(path || "").split("/").filter(Boolean).pop() || path;
    const bits = [pending.folder ? `in ${dir(pending.folder)}` : `in ${session.name}'s folder`];
    if (pending.worktree) bits.push(`new worktree ${pending.worktree.task}`);
    if (pending.launch && pending.launch.model) bits.push(pending.launch.model);
    return bits.join(", ");
  }

  function reviewStart(session, pending) {
    const dialog = sheet("start request", "Start the requested session", `Start it; ${session.name} is told`);
    if (!dialog) return;
    $("#sc-from").textContent = `${session.name} asks to start a ${pending.kind} session ${startWhere(session, pending)}`;
    const name = $("#sc-name");
    const text = $("#sc-text");
    name.value = pending.name || "";
    name.placeholder = "";
    text.value = pending.prompt || "";
    const size = () => {
      $("#sc-size").textContent = `${text.value.length.toLocaleString()} chars`;
    };
    size();
    text.oninput = size;
    const start = $("#sc-start");
    start.onclick = async () => {
      const started = await approveStart(session, pending, { prompt: text.value, name: name.value.trim() }, start);
      if (started) dialog.close("ok");
    };
    dialog.showModal();
    text.focus();
    text.setSelectionRange(0, 0);
    text.scrollTop = 0;
  }

  // A waiting agent start's row: who asks, the new session's name, where
  // it runs, the prompt's first lines, and Decline / Review / Start.
  function startRow({ session, start: pending }) {
    const workspace = workspaceById(session.workspace_id);
    const active = state.attached === session.id;
    const lines = (pending.prompt || "").split("\n");
    const preview = lines.slice(0, HANDOFF_PREVIEW).join("\n") + (lines.length > HANDOFF_PREVIEW ? `\n... ${lines.length - HANDOFF_PREVIEW} more lines` : "");
    const start = el("button", {
      type: "button",
      className: "slab attn-start",
      textContent: "Start",
      title: `Start ${pending.name ? `"${pending.name}"` : "the session"} as ${session.name} asked; it is told`,
      onclick: () => approveStart(session, pending, null, start),
    });
    const drop = el("button", {
      type: "button",
      className: "slab ghost attn-dismiss",
      textContent: "Decline",
      title: `Do not start it; ${session.name} is told`,
      onclick: () => declineStart(session, pending, drop),
    });
    return el(
      "li",
      { dataset: { start: pending.id } },
      el(
        "button",
        {
          type: "button",
          className: `attn-row handoff${active ? " active" : ""}`,
          dataset: { id: session.id },
          title: `Open ${session.name}`,
          onclick: () => openSession(session.id),
        },
        el("span", { className: `glyph ${session.kind}` }, icon(session.kind === "codex" ? "codex" : "claude")),
        el(
          "span",
          { className: "attn-text" },
          el(
            "span",
            { className: "attn-line" },
            el("b", { className: "attn-name", textContent: session.name }),
            pending.name ? el("span", { className: "attn-next", textContent: `+ ${pending.name}`, title: "The new session's name" }) : null,
            el("span", { className: "attn-ws", textContent: workspace ? workspace.name : "" }),
          ),
          el("span", {
            className: "attn-detail",
            textContent: `new session ${startWhere(session, pending)}`,
            title: `${session.name} asks to start a ${pending.kind} session ${startWhere(session, pending)}`,
          }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: "attn-status handoff", textContent: "start" }),
          el("span", { className: "attn-age", textContent: age(pending.created_at) }),
        ),
      ),
      el(
        "div",
        { className: "attn-ask attn-pending" },
        preview.trim() ? el("pre", { className: "attn-handoff-preview", textContent: preview }) : null,
        el(
          "div",
          { className: "attn-handoff-actions" },
          drop,
          el("button", {
            type: "button",
            className: "slab ghost attn-review",
            textContent: "Review",
            title: "Read and edit the whole prompt and the name, then start",
            onclick: () => reviewStart(session, pending),
          }),
          start,
        ),
      ),
    );
  }

  // A lease held past its stuck threshold while sessions wait: who holds
  // it, for how long, what its process is doing, who waits; Keep waiting
  // or Release (confirmed: it breaks the lease and sends SIGTERM to the
  // holder's process group).
  function leaseRow(h) {
    const resource = h.cause.replace(/^lease /, "").replace(/ stuck$/, "");
    const keep = el("button", {
      type: "button",
      className: "slab ghost attn-dismiss",
      textContent: "Keep waiting",
      title: "Leave the lease be; asked again after another threshold",
      onclick: () => answerLease(h, resource, "keep waiting", keep),
    });
    const release = el("button", {
      type: "button",
      className: "slab attn-start",
      textContent: "Release",
      title: "Break the lease and send SIGTERM to its holder's process group",
      onclick: () => answerLease(h, resource, "release", release),
    });
    return el(
      "li",
      { dataset: { hold: h.id } },
      el(
        "button",
        {
          type: "button",
          className: "attn-row handoff",
          dataset: { id: h.session || "" },
          title: h.session ? `Open ${h.session_name || "the waiting session"}` : h.cause,
          onclick: () => h.session && openSession(h.session),
        },
        el("span", { className: "glyph count", textContent: "!" }),
        el(
          "span",
          { className: "attn-text" },
          el("span", { className: "attn-line" }, el("b", { className: "attn-name", textContent: h.cause, title: h.cause })),
          el("span", { className: "attn-detail", textContent: h.session_name ? `${h.session_name} waits` : "", title: h.question }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: "attn-status group awaiting_approval", textContent: "lease" }),
          el("span", { className: "attn-age", textContent: age(new Date(h.since).toISOString()) }),
        ),
      ),
      el(
        "div",
        { className: "attn-ask attn-pending" },
        el("pre", { className: "attn-handoff-preview", textContent: h.question }),
        el("div", { className: "attn-handoff-actions" }, keep, release),
      ),
    );
  }

  async function answerLease(h, resource, answer, button) {
    if (answer === "release") {
      const sure = ctx.confirm
        ? await ctx.confirm({
            kicker: "lease",
            title: `Release ${resource}?`,
            body: "This breaks the lease and sends SIGTERM to its holder's process group: the run holding it stops, and the next in line gets it.",
            ok: "Release",
          })
        : true;
      if (!sure) return;
    }
    if (button) button.disabled = true;
    try {
      const done = await ctx.api("POST", "/api/holds/answer", { holds: [h.id], answer });
      const result = (done.results || [])[0] || {};
      if (result.ok === false) ctx.toast(`Not done: ${result.error}`, { tone: "bad" });
      else ctx.toast(result.said || (answer === "release" ? `Released ${resource}` : `Keeping ${resource}`));
    } catch (error) {
      ctx.toast(`Not done: ${error.message}`, { tone: "bad" });
    } finally {
      if (button) button.disabled = false;
      holdsAt = 0;
      refreshHolds(holdsSig);
    }
  }

  const END_WHY = {
    not_owner: "not its owner",
    start_policy_ask: "its start policy asks you",
    both: "not its owner; its start policy asks you",
  };

  async function approveEnd(session, pending, target, button) {
    if (button) button.disabled = true;
    try {
      await ctx.api("POST", `/api/sessions/${encodeURIComponent(session.id)}/ends/${encodeURIComponent(pending.id)}`, {});
      ctx.toast(`${pending.replace ? "Replacing" : "Ending"} ${target} for ${session.name}`);
      if (ctx.refresh) await ctx.refresh();
    } catch (error) {
      ctx.toast(`Not approved: ${error.message}`, { tone: "bad" });
    } finally {
      if (button) button.disabled = false;
    }
  }

  async function declineEnd(session, pending, target, button) {
    if (button) button.disabled = true;
    try {
      await ctx.api("DELETE", `/api/sessions/${encodeURIComponent(session.id)}/ends/${encodeURIComponent(pending.id)}`);
      ctx.toast(`Declined; ${target} keeps running`);
      if (ctx.refresh) await ctx.refresh();
    } catch (error) {
      ctx.toast(`Not declined: ${error.message}`, { tone: "bad" });
    } finally {
      if (button) button.disabled = false;
    }
  }

  // An end (or replacement) an agent asks for: who asks, which session,
  // why it waits on you, the final prompt's first lines, and Decline /
  // Approve. Approving starts the end: it still waits for the session's
  // turn to finish and nothing to run.
  function endRow({ session, end: pending }) {
    const workspace = workspaceById(session.workspace_id);
    const active = state.attached === session.id;
    const targetSession = state.sessions.find((s) => s.id === pending.target);
    const target = targetSession ? targetSession.name : pending.target.slice(0, 8);
    const verb = pending.replace ? `replace ${target} with ${pending.replace.name}` : `end ${target}`;
    const text = pending.replace
      ? `${pending.final_prompt || ""}\n\n-- ${pending.replace.name}'s prompt --\n${pending.replace.prompt}`
      : pending.final_prompt || (pending.mode === "at_rest" ? "(at its next rest, no final prompt)" : "");
    const lines = text.split("\n");
    const preview = lines.slice(0, HANDOFF_PREVIEW).join("\n") + (lines.length > HANDOFF_PREVIEW ? `\n... ${lines.length - HANDOFF_PREVIEW} more lines` : "");
    const approve = el("button", {
      type: "button",
      className: "slab attn-start",
      textContent: "Approve",
      title: `${pending.replace ? "Replace" : "End"} ${target} as ${session.name} asks: once its turn is over and nothing runs; ${session.name} is told`,
      onclick: () => approveEnd(session, pending, target, approve),
    });
    const drop = el("button", {
      type: "button",
      className: "slab ghost attn-dismiss",
      textContent: "Decline",
      title: `${target} keeps running; ${session.name} is told`,
      onclick: () => declineEnd(session, pending, target, drop),
    });
    return el(
      "li",
      { dataset: { end: pending.id } },
      el(
        "button",
        {
          type: "button",
          className: `attn-row handoff${active ? " active" : ""}`,
          dataset: { id: session.id },
          title: `Open ${session.name}`,
          onclick: () => openSession(session.id),
        },
        el("span", { className: `glyph ${session.kind}` }, icon(session.kind === "codex" ? "codex" : "claude")),
        el(
          "span",
          { className: "attn-text" },
          el(
            "span",
            { className: "attn-line" },
            el("b", { className: "attn-name", textContent: session.name }),
            el("span", { className: "attn-next", textContent: `\u00d7 ${target}`, title: "The session it would end" }),
            el("span", { className: "attn-ws", textContent: workspace ? workspace.name : "" }),
          ),
          el("span", {
            className: "attn-detail",
            textContent: `${verb} (${END_WHY[pending.why] || pending.why})`,
            title: `${session.name} asks to ${verb}`,
          }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: "attn-status handoff", textContent: pending.replace ? "replace" : "end" }),
          el("span", { className: "attn-age", textContent: age(pending.created_at) }),
        ),
      ),
      el(
        "div",
        { className: "attn-ask attn-pending" },
        preview.trim() ? el("pre", { className: "attn-handoff-preview", textContent: preview }) : null,
        el("div", { className: "attn-handoff-actions" }, drop, approve),
      ),
    );
  }

  // A pending handoff's row: who proposes it, the new session's name, why,
  // the prompt's first lines, and Start / Review / Dismiss.
  function handoffRow({ session, handoff: pending }) {
    const workspace = workspaceById(session.workspace_id);
    const active = state.attached === session.id;
    const lines = pending.prompt.split("\n");
    const preview = lines.slice(0, HANDOFF_PREVIEW).join("\n") + (lines.length > HANDOFF_PREVIEW ? `\n... ${lines.length - HANDOFF_PREVIEW} more lines` : "");
    const start = el("button", {
      type: "button",
      className: "slab attn-start",
      textContent: "Start",
      title: `Start ${pending.name ? `"${pending.name}"` : "the new session"} here with this prompt; it continues ${session.name}'s work`,
      onclick: () => startSuccessor(session, pending, null, start),
    });
    const drop = el("button", {
      type: "button",
      className: "slab ghost attn-dismiss",
      textContent: "Dismiss",
      title: "Drop this handoff without starting it",
      onclick: () => dismiss(session, pending, drop),
    });
    return el(
      "li",
      { dataset: { handoff: pending.id } },
      el(
        "button",
        {
          type: "button",
          className: `attn-row handoff${active ? " active" : ""}`,
          dataset: { id: session.id },
          title: `Open ${session.name}`,
          onclick: () => openSession(session.id),
        },
        el("span", { className: `glyph ${session.kind}` }, icon(session.kind === "codex" ? "codex" : "claude")),
        el(
          "span",
          { className: "attn-text" },
          el(
            "span",
            { className: "attn-line" },
            el("b", { className: "attn-name", textContent: session.name }),
            pending.name ? el("span", { className: "attn-next", textContent: `\u2192 ${pending.name}`, title: "The new session's name" }) : null,
            el("span", { className: "attn-ws", textContent: workspace ? workspace.name : "" }),
          ),
          el("span", {
            className: "attn-detail",
            textContent: pending.reason || "continue this work in a new session",
            title: pending.reason || "",
          }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: "attn-status handoff", textContent: "hand off" }),
          el("span", { className: "attn-age", textContent: age(pending.created_at) }),
        ),
      ),
      el(
        "div",
        { className: "attn-ask attn-pending" },
        el("pre", { className: "attn-handoff-preview", textContent: preview }),
        el(
          "div",
          { className: "attn-handoff-actions" },
          drop,
          el("button", {
            type: "button",
            className: "slab ghost attn-review",
            textContent: "Review",
            title: "Read and edit the whole prompt and the name, then start",
            onclick: () => review(session, pending),
          }),
          start,
        ),
      ),
    );
  }

  // A handoff from before they were kept on the session (its telemetry
  // ask): the new session's prompt, editable, and the button that starts
  // it.
  function handoffBox(session, ask) {
    const area = el("textarea", {
      className: "attn-handoff-text",
      rows: 5,
      spellcheck: false,
      "aria-label": `${session.name}'s prompt for the new session`,
      value: handoffs.has(session.id) ? handoffs.get(session.id) : ask.prompt || "",
      oninput: (event) => handoffs.set(session.id, event.target.value),
    });
    const start = el("button", {
      type: "button",
      className: "slab attn-start",
      textContent: "Start session",
      title: "Start a new session here with this prompt; it continues this one's work",
      onclick: () => startSuccessor(session, null, { prompt: area.value }, start),
    });
    return el("div", { className: "attn-handoff attn-reply" }, area, start);
  }

  // An ask answered in place: its command to copy, its options to pick,
  // and a one-line reply.
  function askBox(session, ask) {
    const field = el("input", {
      type: "text",
      className: "attn-reply-input",
      placeholder: ask.kind === "run" ? "Paste the output, or reply" : "Reply",
      "aria-label": `Reply to ${session.name}`,
      value: drafts.get(session.id) || "",
      oninput: (event) => drafts.set(session.id, event.target.value),
    });
    const form = el(
      "form",
      {
        className: "attn-reply",
        onsubmit: (event) => {
          event.preventDefault();
          reply(session, field.value, form);
        },
      },
      field,
      el("button", { type: "submit", className: "slab attn-send", textContent: "Send" }),
    );
    return el(
      "div",
      { className: "attn-ask" },
      ask.command
        ? el(
            "div",
            { className: "attn-cmd" },
            el("pre", { textContent: ask.command }),
            el("button", {
              type: "button",
              className: "slab ghost attn-copy",
              textContent: "Copy",
              title: "Copy the command; run it, then paste its output as the reply",
              onclick: async () => {
                const ok = await ctx.copyText(ask.command);
                ctx.toast(ok ? "Command copied" : "Could not copy", { tone: ok ? "" : "bad" });
              },
            }),
          )
        : null,
      ask.kind === "handoff" && ask.prompt ? handoffBox(session, ask) : null,
      (ask.options || []).length
        ? el(
            "div",
            { className: "attn-options" },
            ask.options.map((option) =>
              el("button", { type: "button", className: "chip attn-option", textContent: option, title: `Reply "${option}"`, onclick: () => reply(session, option) }),
            ),
          )
        : null,
      form,
    );
  }

  // One agent that needs you: its dot, its name, what it needs and since
  // when; what it asks under it (two lines at most). Its reply box opens
  // under the attached one in the sidebar, under every ask on the start
  // screen (`full`). Its other facts are the inspector's (hover, the row's
  // I, `fw hub ls`).
  function row({ session, agent, ask }, full) {
    const active = state.attached === session.id;
    const look = ctx.statusLook ? ctx.statusLook(session) : { dot: ask ? "asking" : agent.state };
    return el(
      "li",
      { dataset: { id: `need:${session.id}` } },
      el(
        "button",
        {
          type: "button",
          className: `attn-row ${ask ? "asking" : agent.state}${active ? " active" : ""}`,
          dataset: { id: session.id },
          "aria-current": active ? "true" : undefined,
          onclick: () => openSession(session.id),
          onmouseenter: (event) => ctx.inspect && ctx.inspect.hover(session.id, event.currentTarget),
          onmouseleave: () => ctx.inspect && ctx.inspect.leave(),
          onkeydown: (event) => {
            if ((event.key === "i" || event.key === "I") && ctx.inspect) {
              event.preventDefault();
              ctx.inspect.toggle(session.id, event.currentTarget);
            }
          },
        },
        el("span", { className: "glyph state" }, el("span", { className: `dot ${look.dot}`, "aria-hidden": "true" })),
        el(
          "span",
          { className: "attn-text" },
          el("span", { className: "attn-line" }, el("b", { className: "attn-name", textContent: session.name })),
          el("span", {
            className: "attn-detail",
            title: ask ? ask.text : agent.detail || "",
          }, needLine(session, agent, ask)),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: `attn-status ${ask ? "asking" : agent.state}`, textContent: ask ? ASK_WORD[ask.kind] || "asks" : agent.state }),
          el("span", { className: "attn-age", textContent: age(ask ? new Date(ask.since).toISOString() : agent.since) }),
        ),
      ),
      ask && (full || active) ? askBox(session, ask) : null,
    );
  }

  // Finished turns nobody here has looked at: one quiet line (newest
  // first), their rows under it once tapped.
  let finishedOpen = false;
  function finishedRows(done, full) {
    if (!done.length) return [];
    const names = done.map((e) => e.session.name).join(", ");
    return [
      el("li", { dataset: { id: "finished" } },
        el("button", {
          type: "button",
          className: "attn-finished",
          "aria-expanded": String(finishedOpen),
          title: finishedOpen ? "Fold the finished turns" : `Finished, not looked at yet: ${names}`,
          onclick: () => {
            finishedOpen = !finishedOpen;
            if (ctx.rerender) ctx.rerender();
            else render();
          },
        },
        el("span", { className: "dot agent-done", "aria-hidden": "true" }),
        el("b", { textContent: `${done.length} finished` }),
        el("span", { className: "attn-finished-names", textContent: finishedOpen ? "" : names }))),
      ...(finishedOpen ? done.map((e) => row(e, full)) : []),
    ];
  }

  // A flywheel task that needs you: opens its folder's panel.
  function flywheelRow({ folder, label, alert, count }) {
    return el(
      "li",
      {},
      el(
        "button",
        {
          type: "button",
          className: `attn-row fw ${alert.kind}`,
          onclick: () => flywheel.open(folder),
          title: count > 1
            ? `${label}: ${alert.detail}. Open its Flywheel panel.`
            : `${alert.subject} in ${label}: ${alert.detail}`,
        },
        el("span", { className: "glyph fw" }, icon("wheel")),
        el(
          "span",
          { className: "attn-text" },
          el(
            "span",
            { className: "attn-line" },
            el("b", { className: "attn-name", textContent: alert.subject }),
            el("span", { className: "attn-ws", textContent: label }),
          ),
          el("span", { className: "attn-detail", textContent: alert.detail }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: `attn-status fw ${alert.kind}`, textContent: alert.kind }),
        ),
      ),
    );
  }

  // The sidebar's list, and the phone's start screen (no session open:
  // the drawer is closed, so the list is the screen; app.css shows it on
  // narrow screens only).
  // A reply being typed is never torn down under the user: the render
  // waits for the field to lose focus.
  let held = false;
  // The sessions the list shows now (frozen with it while a reply is
  // typed): the Everything list leaves out exactly these, so a session is
  // never in both.
  let shown = new Set();
  // Pending handoffs and agent starts the list shows now (rows of their
  // own: their sessions stay in the Everything list).
  let pendingCount = 0;
  const typing = () => !!(document.activeElement && document.activeElement.closest(".attn-reply"));
  document.addEventListener("focusout", (event) => {
    if (!held || !event.target.closest || !event.target.closest(".attn-reply")) return;
    // Focus moving to the row's own Send button is not leaving it.
    setTimeout(() => {
      if (held && !typing()) {
        held = false;
        // The whole sidebar: the list below follows what this shows.
        if (ctx.rerender) ctx.rerender();
        else render();
      }
    }, 0);
  });

  function render() {
    if (typing()) {
      held = true;
      return;
    }
    held = false;
    const all = entries();
    const passed = handoffEntries();
    const allStarts = startEntries();
    const allEnds = endEntries();
    // What waits changed: re-read the holds (their groups).
    refreshHolds(
      [
        ...all.filter((e) => e.level < 2).map((e) => `${e.session.id}:${e.ask ? e.ask.since : e.agent.since}`),
        ...allStarts.map((e) => e.start.id),
        ...allEnds.map((e) => e.end.id),
        // A lease turning stuck (or not) shows on its sessions' views.
        ...state.sessions.flatMap((s) => ((s.leases && s.leases.stuck) || []).map((x) => x.lease.id)),
        flywheel ? flywheel.attention().length : 0,
      ].join(","),
    );
    // Rows a group stands for step aside.
    const inGroup = grouped();
    const list = all.filter((e) =>
      e.level === 0 ? !inGroup.blocked.has(e.session.id) : e.level === 1 ? !inGroup.ids.has(`ask:${e.session.id}:${e.ask.since}`) : true,
    );
    const starts = allStarts.filter((e) => !inGroup.ids.has(`start:${e.session.id}:${e.start.id}`));
    const ends = allEnds.filter((e) => !inGroup.ids.has(`end:${e.session.id}:${e.end.id}`));
    const groups = holdGroups;
    shown = new Set(list.map((e) => e.session.id));
    for (const g of groups) for (const h of g.holds) if (h.session && !/^(start|end):/.test(h.id)) shown.add(h.session);
    pendingCount = leaseHolds.length + passed.length + starts.length + ends.length + groups.reduce((n, g) => n + g.holds.filter((h) => /^(start|end):/.test(h.id)).length, 0);
    const loop = flywheel ? flywheel.attention() : [];
    const graded = fanout ? fanout.attention() : [];
    const queues = ctx.queue ? ctx.queue().attention() : [];
    for (const root of [$("#attention"), $("#empty-attention")]) {
      if (!root) continue;
      const any = leaseHolds.length || groups.length || list.length || passed.length || starts.length || ends.length || loop.length || graded.length || queues.length;
      root.hidden = !any;
      morph(root, any ? content(list, passed, starts, ends, loop, graded, queues, groups, root.id === "empty-attention", leaseHolds) : []);
    }
  }

  function content(list, passed, starts, ends, loop, graded, queues, groups, full, leases = []) {
    const blocked = list.filter((e) => e.level === 0).length;
    const asking = list.filter((e) => e.level === 1).length;
    const failed = list.filter((e) => e.level === 2 && e.agent.state === "failed").length;
    const alerts = loop.reduce((n, e) => n + (e.count || 1), 0);
    // The heading's counts, on one line.
    const said = [
      leases.length ? `${leases.length} lease${leases.length === 1 ? "" : "s"} stuck` : "",
      groups.length ? `${groups.reduce((n, g) => n + g.holds.length, 0)} in ${groups.length} group${groups.length === 1 ? "" : "s"}` : "",
      blocked ? `${blocked} blocked` : "",
      passed.length ? `${passed.length} handoff${passed.length === 1 ? "" : "s"}` : "",
      starts.length ? `${starts.length} start${starts.length === 1 ? "" : "s"}` : "",
      ends.length ? `${ends.length} end${ends.length === 1 ? "" : "s"}` : "",
      asking ? `${asking} asking` : "",
      failed ? `${failed} failed` : "",
      alerts ? `${alerts} flywheel` : "",
      graded.length ? `${graded.length} fan-out${graded.length === 1 ? "" : "s"}` : "",
      queues.length ? `${queues.length} queue${queues.length === 1 ? "" : "s"} stalled` : "",
    ]
      .filter(Boolean)
      .join(" · ");
    return [
      el(
        "header",
        { className: "attn-head" },
        el("span", { className: "attn-title", textContent: "Needs you" }),
        el("span", { className: "attn-count", textContent: said, title: said }),
      ),
      // Blocked agents, handoffs, asks, then fan-out verdicts, then
      // finished turns.
      el(
        "ul",
        { className: "attn-list" },
        leases.map(leaseRow),
        groups.map((g) => groupRow(g, full)),
        list.filter((e) => e.level === 0).map((e) => row(e, full)),
        passed.map(handoffRow),
        starts.map(startRow),
        ends.map(endRow),
        list.filter((e) => e.level === 1).map((e) => row(e, full)),
        queues.map((s) => ctx.queue().attentionRow(s)),
        graded.map((f) => fanout.attentionRow(f)),
        // A failed turn is acted on: a row of its own, not folded.
        list.filter((e) => e.level === 2 && e.agent.state === "failed").map((e) => row(e, full)),
        finishedRows(list.filter((e) => e.level === 2 && e.agent.state !== "failed"), full),
        loop.map(flywheelRow),
      ),
    ];
  }

  // Ages tick while the list shows.
  setInterval(() => {
    if (document.visibilityState === "visible" && !$("#attention").hidden) {
      if (ctx.tick) ctx.tick();
      else if (ctx.rerender) ctx.rerender();
      else render();
    }
  }, 30000);

  return { render, entries, branchOf, shownIds: () => shown, handoffCount: () => pendingCount };
};

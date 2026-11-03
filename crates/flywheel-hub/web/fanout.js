// flywheel hub: verified fan-out. One prompt to 2-6 agents (claude and/or
// codex, a model each), each in its own sibling worktree cut from one base
// commit; grader commands the hub runs itself in every worktree once its
// agent reports a finished turn (the agents never see them). The board
// ranks the candidates -- every grader passing first, then the smaller
// diff, then the lower cost -- and says why; from it: open a candidate or
// its review drawer, re-grade, ask a failing one to fix (its grader output
// is pasted into the agent), pick the winner (merged into the base through
// the review merge path; a rebase is offered when the base moved, which
// grades again), then remove the losers' worktrees (refused where unique
// work would be lost unless forced) or just stop their sessions. A
// fan-out whose graders finished joins Needs you until its board is
// viewed. Promote to a flywheel task and the task-seeded launch live in
// fanout-task.js (ctx.task). app-base.js creates it with its helpers;
// protocol: /api/fanouts* and the events socket's {"type":"fanout"}
// pushes. This hub's fan-outs only (machines' are reached on their own
// hub).
"use strict";

window.FwHubFanout = function (ctx) {
  const { $, el, icon, api, state, age, sessionById, workspaceById, folderLabel, openSession, toast } = ctx;

  const SEEN_KEY = "fw-hub.fanout-seen";
  const MIN = 2;
  const MAX = 6;
  const MODELS = {
    claude: ["opus", "sonnet", "haiku", "fable", "claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
    codex: ["gpt-5-codex", "gpt-5", "gpt-5.1-codex", "gpt-5.1"],
  };
  const CLAUDE_MODES = [
    ["", "CLI default (asks)"],
    ["acceptEdits", "acceptEdits: edits without asking"],
    ["auto", "auto: a classifier approves safe actions"],
    ["dontAsk", "dontAsk: deny whatever would ask"],
    ["bypassPermissions", "bypassPermissions: no checks at all"],
  ];
  const CODEX_APPROVALS = [["", "CLI default"], ["on-request", "on-request"], ["on-failure", "on-failure"], ["never", "never"]];
  const CODEX_SANDBOXES = [["", "CLI default"], ["workspace-write", "workspace-write"], ["read-only", "read-only"], ["danger-full-access", "danger-full-access"]];
  const VERDICT = { pass: "pass", fail: "fail", stale: "stale", pending: "grading", none: "—" };
  const PHASE = {
    working: "working",
    blocked: "waits on you",
    waiting: "waiting",
    queued: "queued",
    grading: "grading",
    graded: "graded",
    ended: "ended",
    removed: "removed",
  };

  const ui = {
    enabled: true,
    runner: null,
    list: [],
    open: null, // id of the fan-out the board shows
    full: null, // its full board (GET /api/fanouts/{id}); the list holds summaries
    error: "",
    busy: new Set(), // "<id>/<label>/<action>" running now
    seq: 0,
  };

  let seen = {};
  try {
    seen = JSON.parse(window.localStorage.getItem(SEEN_KEY) || "{}") || {};
  } catch (_) {
    seen = {};
  }
  function saveSeen() {
    try {
      window.localStorage.setItem(SEEN_KEY, JSON.stringify(seen));
    } catch (_) {
      // Private mode: marks last this page only.
    }
  }

  const byId = (id) => ui.list.find((f) => f.id === id) || null;
  // The board's fan-out: its full view once fetched, else its summary.
  const shownFanout = () => (ui.full && ui.full.id === ui.open ? ui.full : byId(ui.open));
  const short = (sha) => (sha || "").slice(0, 10);
  const { tokens, dollars, pathTail } = window.FwHubFmt;
  const money = (usd) => (usd === null || usd === undefined ? null : dollars(usd));
  const agentLabel = (c) => (c.model ? `${c.kind}:${c.model}` : c.kind);

  // --- dialogs (built once) -------------------------------------------

  function sheet(id, className, label) {
    const dialog = el("dialog", { id, className: `sheet ${className}`, "aria-label": label });
    dialog.addEventListener("click", (event) => {
      if (event.target.closest("[data-close]") || event.target === dialog) dialog.close("cancel");
    });
    dialog.addEventListener("close", () => {
      if (!document.querySelector("dialog[open]")) ctx.focusTerminal();
    });
    document.body.append(dialog);
    return dialog;
  }
  const board = sheet("dlg-fanout", "fo-sheet", "Verified fan-out");
  const launch = sheet("dlg-fanout-new", "fo-new", "New verified fan-out");
  const act = sheet("dlg-fanout-act", "narrow fo-act", "Fan-out action");
  board.addEventListener("close", () => {
    ui.open = null;
    ui.full = null;
  });

  const closeButton = (what) =>
    el("button", { type: "button", className: "icon", "data-close": true, title: "Close (Esc)", "aria-label": `Close ${what}` }, icon("x"));

  // A confirmation over the board: `body` nodes, then buttons; resolves
  // with the button's value ("" on cancel).
  function ask({ kicker, title, body, buttons }) {
    act.replaceChildren(
      el(
        "form",
        { method: "dialog" },
        el("header", { className: "sheet-head" }, el("span", { className: "sheet-kicker", textContent: kicker || "fan-out" }), el("h2", { textContent: title })),
        el("div", { className: "fo-act-body" }, body),
        el(
          "footer",
          { className: "sheet-foot" },
          el("button", { type: "submit", className: "slab ghost", value: "", textContent: (buttons || []).length ? "Cancel" : "Close" }),
          (buttons || []).map((b) => el("button", { type: "submit", className: `slab${b.tone ? ` ${b.tone}` : ""}`, value: b.value, textContent: b.label })),
        ),
      ),
    );
    act.returnValue = "";
    act.showModal();
    const primary = act.querySelector(".sheet-foot .slab:not(.ghost)");
    (primary || act.querySelector(".sheet-foot .slab")).focus();
    return new Promise((resolve) => act.addEventListener("close", () => resolve(act.returnValue || ""), { once: true }));
  }

  const commandList = (plan) =>
    el(
      "div",
      { className: "fo-plan" },
      plan.summary ? el("p", { className: "fo-plan-summary", textContent: plan.summary }) : null,
      (plan.details || []).map((d) => el("p", { className: "fo-plan-detail", textContent: d })),
      (plan.warnings || []).map((w) => el("p", { className: "fo-plan-warn", textContent: `warning: ${w}` })),
      (plan.commands || []).length
        ? el("pre", { className: "fo-cmds", textContent: plan.commands.map((c) => `$ ${c.display}`).join("\n") })
        : null,
    );

  // --- data -----------------------------------------------------------

  async function reload() {
    const seq = ++ui.seq;
    let listed;
    try {
      listed = await api("GET", "/api/fanouts");
    } catch (error) {
      if (seq !== ui.seq) return;
      ui.error = error.message;
      if (board.open) renderBoard();
      return;
    }
    if (seq !== ui.seq) return;
    ui.error = "";
    ui.enabled = listed.enabled !== false;
    ui.runner = listed.runner || null;
    ui.list = listed.fanouts || [];
    for (const id of Object.keys(seen)) if (!byId(id)) delete seen[id];
    saveSeen();
    if (board.open) {
      const shown = byId(ui.open);
      if (shown && shown.graded_at) markSeen(shown);
      renderBoard();
      await loadFull(ui.open);
    }
    ctx.onChange();
  }

  // The open board's full view (grader output, fresh worktree reads).
  async function loadFull(id) {
    if (!id || !byId(id)) return;
    let full;
    try {
      full = await api("GET", `/api/fanouts/${id}`);
    } catch (_) {
      return;
    }
    if (ui.open !== id) return;
    ui.full = full;
    if (board.open) renderBoard();
  }

  let reloadTimer = null;
  function reloadSoon() {
    clearTimeout(reloadTimer);
    reloadTimer = setTimeout(() => reload(), 200);
  }

  function onEvent(message) {
    if (message.type === "fanout") reloadSoon();
  }

  // --- Needs you ------------------------------------------------------

  function markSeen(f) {
    if (f.graded_at && seen[f.id] !== f.graded_at) {
      seen[f.id] = f.graded_at;
      saveSeen();
    }
  }

  function headline(f) {
    const c = f.counts;
    if (f.winner) return `${f.winner.label} merged into ${f.base}`;
    const lead = f.leader ? `${f.leader} leads (passes)` : c.failing ? "none pass yet" : "";
    const rest = [];
    if (c.grading) rest.push(`${c.grading} grading`);
    if (c.running) rest.push(`${c.running} at work`);
    return [lead, `${c.passing}/${c.candidates} pass`, ...rest].filter(Boolean).join(" · ");
  }

  // Fan-outs whose graders finished since this browser last looked (and
  // that have no winner yet), newest first.
  function attention() {
    return ui.list.filter((f) => !f.winner && f.graded_at && seen[f.id] !== f.graded_at);
  }

  // Sessions that are some fan-out's candidates (the hub grades their
  // finished turns: Needs you lists the fan-out instead).
  function candidateSessions() {
    const ids = new Set();
    for (const f of ui.list) for (const c of f.candidates) if (c.phase !== "removed") ids.add(c.session_id);
    return ids;
  }

  function attentionRow(f) {
    const passing = f.counts.passing > 0;
    return el(
      "li",
      {},
      el(
        "button",
        {
          type: "button",
          className: `attn-row fo${passing ? " pass" : " fail"}`,
          onclick: () => open(f.id),
          title: `Fan-out ${f.name}: ${headline(f)}. Open its board.`,
        },
        el("span", { className: "glyph fo" }, icon("branch")),
        el(
          "span",
          { className: "attn-text" },
          el("span", { className: "attn-line" }, el("b", { className: "attn-name", textContent: f.name }), el("span", { className: "attn-ws", textContent: f.workspace || "" })),
          el("span", { className: "attn-detail", textContent: `graded: ${headline(f)}` }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: `attn-status fo${passing ? " pass" : ""}`, textContent: "fan-out" }),
          el("span", { className: "attn-age", textContent: age(f.graded_at) }),
        ),
      ),
    );
  }

  // --- the board ------------------------------------------------------

  function open(id) {
    const target = byId(id) || ui.list[0];
    if (!target) {
      openLaunch();
      return;
    }
    if (ui.open !== target.id) ui.full = null;
    ui.open = target.id;
    markSeen(target);
    document.body.classList.remove("side-open");
    renderBoard();
    if (!board.open) board.showModal();
    ctx.onChange();
    reloadSoon();
  }

  const isBusy = (f, c, what) => ui.busy.has(`${f.id}/${c ? c.label : ""}/${what}`);

  async function busy(f, c, what, run) {
    const key = `${f.id}/${c ? c.label : ""}/${what}`;
    if (ui.busy.has(key)) return;
    ui.busy.add(key);
    renderBoard();
    try {
      await run();
    } catch (error) {
      toast(error.message, { tone: "bad", timeout: 7000 });
    } finally {
      ui.busy.delete(key);
      renderBoard();
    }
  }

  function receipt(g) {
    const ok = g.status === "passed";
    const skipped = g.status === "skipped";
    const out = [g.stdout, g.stderr].some((t) => t && t.trim());
    const label = g.name || g.run;
    const line = el(
      "span",
      { className: "fo-rcpt-line" },
      el("span", { className: `fo-rcpt-dot ${g.status}`, "aria-hidden": "true" }),
      el("b", { textContent: label, title: g.run }),
      el("span", {
        className: "fo-rcpt-state",
        textContent: ok ? `passed · ${(g.duration_ms / 1000).toFixed(1)}s` : skipped ? "skipped" : `${g.status.replace("_", " ")}${g.exit_code !== undefined ? ` · exit ${g.exit_code}` : ""} · ${(g.duration_ms / 1000).toFixed(1)}s`,
      }),
    );
    if (ok || skipped || !out) return el("li", { className: `fo-rcpt ${g.status}` }, line);
    return el(
      "li",
      { className: `fo-rcpt ${g.status}` },
      el(
        "details",
        {},
        el("summary", {}, line),
        el("pre", { className: "fo-out", textContent: `$ ${g.run}\n${g.stdout ? g.stdout.trimEnd() + "\n" : ""}${g.stderr ? `--- stderr ---\n${g.stderr.trimEnd()}` : ""}` }),
      ),
    );
  }

  function actionButton(label, title, onclick, { tone = "ghost", disabled = false, pending = false } = {}) {
    return el("button", {
      type: "button",
      className: `slab ${tone}${pending ? " busy" : ""}`,
      textContent: pending ? `${label}…` : label,
      title,
      disabled: disabled || pending,
      onclick: (event) => {
        event.stopPropagation();
        onclick();
      },
    });
  }

  const whenText = (at, rest) => `graded ${age(at)} ago${rest || ""}`;
  // Ages on the board tick (without a re-render, which would fold open
  // grader output).
  setInterval(() => {
    if (!board.open || document.visibilityState !== "visible") return;
    for (const node of board.querySelectorAll(".fo-when[data-at]")) node.textContent = whenText(node.dataset.at, node.dataset.rest);
  }, 10000);

  // Why a passing candidate cannot be picked (the hub says exactly why
  // when asked; this is the button's hint).
  function unpickable(c) {
    const g = c.grade;
    if (!g) return "it has not been graded";
    if (g.diff.files === 0) return "it changes nothing against the base: there is nothing to merge";
    if (g.dirty || g.untracked) return "it was graded with uncommitted changes: commit them (Commit in Review), then Re-grade";
    if (c.stale) return `${c.stale}: Re-grade it`;
    return "it cannot be picked now";
  }

  function card(f, c) {
    const g = c.grade;
    const session = sessionById(c.session_id);
    const meta = [];
    meta.push(el("span", { className: "fo-branch", title: c.path }, icon("branch"), el("span", { textContent: c.branch })));
    if (g) {
      meta.push(
        el("span", { className: "fo-diff", title: `${g.diff.files} files changed vs the base (${g.diff.untracked} untracked)` },
          el("b", { className: "ins", textContent: `+${g.diff.insertions}` }),
          el("b", { className: "del", textContent: `−${g.diff.deletions}` }),
          el("span", { textContent: `${g.diff.files} file${g.diff.files === 1 ? "" : "s"}` })),
      );
    }
    const cost = money(c.cost_usd);
    if (cost || c.tokens) meta.push(el("span", { className: "fo-cost", textContent: [cost || (c.tokens ? "unpriced" : null), c.tokens ? `${tokens(c.tokens)} tok` : null].filter(Boolean).join(" · ") }));
    if (g) {
      const rest = `${c.gradings > 1 ? ` · ${c.gradings}×` : ""}${c.fixes ? ` · ${c.fixes} fix${c.fixes === 1 ? "" : "es"} asked` : ""}`;
      meta.push(el("span", { className: "fo-when", dataset: { at: g.finished_at, rest }, textContent: whenText(g.finished_at, rest), title: `trigger: ${g.trigger}` }));
    }

    const actions = [];
    // Why a passing candidate's Pick winner is off, on the card.
    let blocked = "";
    if (c.phase !== "removed") {
      if (session && session.live) actions.push(actionButton("Open", "Show its terminal", () => { board.close(); openSession(c.session_id); }));
      if (session) actions.push(actionButton("Review", "Its commits and diffs (review drawer)", () => { board.close(); ctx.openReview(c.session_id); }));
      if (!f.winner) {
        actions.push(actionButton("Re-grade", "Run the graders again now", () => regrade(f, c), { pending: isBusy(f, c, "grade"), disabled: c.verdict === "pending" || !ui.runner }));
        // An operator-side failure (a changed grader script) is not the
        // agent's to fix: the card's error says what to do instead.
        if (g && !g.passed && !g.operator && session && session.state === "running" && c.verdict !== "pending") {
          actions.push(actionButton("Ask to fix", "Paste what failed into its agent; it is graded again when it finishes", () => fix(f, c), { pending: isBusy(f, c, "fix") }));
        }
        if (c.verdict === "pass") {
          actions.push(actionButton("Pick winner", c.pickable ? `Merge ${c.branch} into ${f.base}` : unpickable(c), () => pick(f, c), { tone: "", pending: isBusy(f, c, "pick"), disabled: !c.pickable }));
          if (!c.pickable) blocked = unpickable(c);
          if (!c.pickable && g && (g.dirty || g.untracked) && session) {
            actions.push(actionButton("Commit in Review…", "Commit its changes (Review, Commit all), then Re-grade", () => { board.close(); ctx.openReview(c.session_id); }, { tone: "" }));
          }
        }
      }
    }

    return el(
      "li",
      { className: `fo-card ${c.verdict}${c.winner ? " winner" : ""}${c.phase === "removed" ? " gone" : ""}`, dataset: { label: c.label } },
      el("div", { className: "fo-rank", "aria-label": `rank ${c.rank}` }, el("span", { textContent: String(c.rank) })),
      el(
        "div",
        { className: "fo-main" },
        el(
          "div",
          { className: "fo-line" },
          el("b", { className: "fo-label", textContent: c.label }),
          el("span", { className: `glyph ${c.kind}` }, icon(c.kind === "codex" ? "codex" : "claude")),
          el("span", { className: "fo-agent", textContent: agentLabel(c) }),
          el("span", { className: `fo-verdict ${c.verdict}`, textContent: c.winner ? "winner" : VERDICT[c.verdict] || c.verdict }),
          el("span", { className: `fo-phase ${c.phase}`, textContent: PHASE[c.phase] || c.phase }),
          c.session ? el("span", { className: "fo-session", textContent: c.session }) : null,
        ),
        el("div", { className: "fo-meta" }, meta),
        el("ul", { className: "fo-why" }, (c.why || []).map((w) => el("li", { textContent: w }))),
        g && g.graders.length ? el("ul", { className: "fo-rcpts" }, g.graders.map(receipt)) : null,
        g && g.error ? el("p", { className: "fo-error", textContent: `grading failed: ${g.error}` }) : null,
        blocked ? el("p", { className: "fo-blocked", textContent: `Not pickable yet: ${blocked}.` }) : null,
        actions.length ? el("div", { className: "fo-actions" }, actions) : null,
      ),
    );
  }

  function renderBoard() {
    if (!board.open && !ui.open) return;
    const f = shownFanout();
    const switcher = ui.list.length > 1
      ? el(
        "select",
        {
          className: "fo-switch",
          "aria-label": "Fan-out",
          onchange: (event) => open(event.target.value),
        },
        ui.list.map((x) => el("option", { value: x.id, textContent: `${x.name} — ${x.workspace || "?"}`, selected: x.id === ui.open })),
      )
      : null;
    const head = el(
      "header",
      { className: "sheet-head fo-head" },
      el(
        "div",
        { className: "fo-title" },
        el("span", { className: "sheet-kicker", textContent: "verified fan-out" }),
        el("h2", { id: "fo-title", textContent: f ? f.name : "Fan-outs" }),
        f
          ? el("span", { className: "fo-sub" },
            el("span", { textContent: `${f.workspace || "?"} · ${f.base} @ ${short(f.base_sha)}` }),
            el("span", { textContent: ` · ${f.candidates.length} agents · ${headline(f)}` }))
          : null,
      ),
      el(
        "div",
        { className: "fo-head-actions" },
        switcher,
        f ? el("button", { type: "button", className: "slab ghost", textContent: "Promote", title: "Promote to flywheel task: write it as a task of the repo's directory work source, for fw worker to verify again", onclick: () => ctx.task().promote(f) }) : null,
        el("button", { type: "button", className: "slab ghost", textContent: "New", title: "New verified fan-out", onclick: () => openLaunch(f && f.workspace_id) }),
        closeButton("the fan-out board"),
      ),
    );
    if (!f) {
      board.replaceChildren(head, el("div", { className: "fo-body" }, el("p", { className: "fo-empty", textContent: ui.error || "No fan-outs yet. Launch one: one prompt, several agents, graded worktrees." })));
      return;
    }
    const graders = el(
      "div",
      { className: "fo-graders" },
      el("span", { className: "field-label", textContent: "graders" }),
      f.graders.map((g, i) => el("code", { className: `fo-grader${g.held_out ? " held" : ""}`, title: g.held_out ? `held out: never shown to the agents, not even when asking to fix\n${g.run}` : g.run, textContent: `${i + 1}. ${g.held_out ? "[held out] " : ""}${g.name ? `${g.name}: ` : ""}${g.run}` })),
      el("span", { className: "fo-note", textContent: `run by the hub (${f.runner || "no runner"}) in each worktree, ${f.timeout_secs}s each; not added to the prompt${f.origin ? " (the task's own prompt lists its visible graders; held-out ones never)" : ""}` }),
      f.protect.length ? el("span", { className: "fo-note", textContent: `protected: ${f.protect.join(", ")}` }) : null,
      ctx.task().boardNotes(f),
    );
    const warnings = (f.warnings || []).length
      ? el("div", { className: "fo-banner warn" }, f.warnings.map((w) => el("p", { textContent: w })))
      : null;
    const winner = f.winner ? f.candidates.find((c) => c.label === f.winner.label) : null;
    const banner = f.winner
      ? el(
        "div",
        { className: "fo-banner won" },
        el("span", { textContent: `${f.winner.label} merged into ${f.winner.base} at ${short(f.winner.head)} (${age(f.winner.at)} ago).` }),
        f.candidates.some((c) => !c.winner && c.phase !== "removed")
          ? actionButton("Clean up the others", "Remove the losing worktrees and their sessions", () => cleanup(f), { tone: "", pending: isBusy(f, null, "cleanup") })
          : winner && winner.phase !== "removed"
            ? actionButton(`Remove ${winner.label}'s worktree`, `It is merged into ${f.base}: remove its worktree and session`, () => cleanupWinner(f, winner), { pending: isBusy(f, null, "cleanup") })
            : null,
      )
      : f.settled && !f.leader
        ? el("div", { className: "fo-banner lost", textContent: "Nothing passes. Ask a candidate to fix, or re-grade after changes." })
        : null;
    const prompt = el(
      "details",
      { className: "fo-prompt" },
      el("summary", { textContent: "prompt" }),
      el("pre", { textContent: f.sent_prompt }),
    );
    const foot = el(
      "footer",
      { className: "fo-foot" },
      el("span", { className: "fo-note fo-where" },
        `created ${age(f.created_at)} ago · `,
        el("span", { className: "path-tail", textContent: pathTail(f.folder || f.repo), title: f.folder || f.repo })),
      el("button", {
        type: "button",
        className: "text-btn danger",
        textContent: "forget this fan-out",
        title: "Drop the record; its sessions and worktrees stay",
        onclick: () => forget(f),
      }),
    );
    board.replaceChildren(
      head,
      el(
        "div",
        { className: "fo-body" },
        ui.error ? el("p", { className: "fo-error", textContent: ui.error }) : null,
        banner,
        warnings,
        graders,
        el("ol", { className: "fo-cards" }, f.candidates.map((c) => card(f, c))),
        prompt,
        foot,
      ),
    );
  }

  // --- actions --------------------------------------------------------

  const post = (f, what, body) => api("POST", `/api/fanouts/${f.id}/${what}`, body);

  function regrade(f, c) {
    return busy(f, c, "grade", async () => {
      await post(f, "grade", { candidate: c.label });
      toast(`${c.label}: grading again.`);
      await reload();
    });
  }

  function fix(f, c) {
    return busy(f, c, "fix", async () => {
      const plan = await post(f, "fix", { candidate: c.label, dry_run: true });
      const go = await ask({
        kicker: "ask to fix",
        title: `Paste into ${plan.session}`,
        body: [
          el("p", { className: "sheet-body", textContent: "This goes to the agent as its next prompt (then Enter); its next finished turn is graded again." }),
          el("pre", { className: "fo-cmds fo-prompt-text", textContent: plan.prompt }),
        ],
        buttons: [{ label: "Paste", value: "go" }],
      });
      if (go !== "go") return;
      await post(f, "fix", { candidate: c.label });
      toast(`Asked ${plan.session} to fix it.`);
      await reload();
    });
  }

  function loserList(items) {
    return el(
      "ul",
      { className: "fo-losers" },
      items.map((i) =>
        el("li", {},
          el("b", { textContent: i.candidate }),
          el("span", { className: "fo-path path-tail", textContent: pathTail(i.path), title: i.path }),
          el("span", { className: `fo-loser-state${i.refusal ? " keep" : ""}`, textContent: i.refusal ? `holds unique work: ${i.refusal}` : "nothing unique: removable" }))),
    );
  }

  function pick(f, c) {
    return busy(f, c, "pick", async () => {
      const plan = await post(f, "pick", { candidate: c.label, dry_run: true });
      if (plan.needs_rebase) {
        const go = await ask({
          kicker: "pick winner",
          title: `Rebase ${c.label} first`,
          body: [
            el("p", { className: "sheet-body", textContent: `${f.base} moved since ${c.label} was cut, so it is not a fast-forward. Rebase it onto ${f.base}: the hub grades it again, and it can be picked once it passes.` }),
            plan.refusal ? el("p", { className: "fo-plan-detail", textContent: plan.refusal }) : null,
          ],
          buttons: [{ label: "Rebase and re-grade", value: "rebase" }],
        });
        if (go === "rebase") await rebase(f, c);
        return;
      }
      if (plan.refusal) {
        await ask({ kicker: "pick winner", title: `${c.label} cannot be merged now`, body: [el("p", { className: "sheet-body", textContent: plan.refusal })], buttons: [] });
        return;
      }
      const go = await ask({
        kicker: "pick winner",
        title: `Merge ${c.label} into ${f.base}`,
        body: [
          commandList(plan.merge),
          plan.losers.length ? el("p", { className: "field-label", textContent: "then, the others" }) : null,
          plan.losers.length ? loserList(plan.losers) : null,
        ],
        buttons: [{ label: "Merge", value: "merge" }],
      });
      if (go !== "merge") return;
      const done = await post(f, "pick", { candidate: c.label, confirm: plan.merge.fingerprint });
      await reload();
      if (!done.merge.ok) {
        await ask({ kicker: "pick winner", title: "The merge stopped", body: [el("pre", { className: "fo-cmds", textContent: (done.merge.ran || []).map((r) => `$ ${r.display}\n${r.output}`).join("\n") || done.merge.summary })], buttons: [] });
        return;
      }
      toast(`${c.label} merged into ${f.base}.`, { timeout: 6000 });
      if (done.losers.length) await cleanup(byId(f.id) || f, done.losers);
    });
  }

  async function rebase(f, c) {
    const report = await post(f, "rebase", { candidate: c.label, dry_run: true });
    const plan = report.plan;
    if (plan.refusal) {
      await ask({ kicker: "rebase", title: `${c.label} cannot be rebased now`, body: [el("p", { className: "sheet-body", textContent: plan.refusal })], buttons: [] });
      return;
    }
    const go = await ask({ kicker: "rebase", title: `Rebase ${c.label} onto ${f.base}`, body: [commandList(plan)], buttons: [{ label: "Rebase", value: "go" }] });
    if (go !== "go") return;
    const done = await post(f, "rebase", { candidate: c.label, confirm: plan.fingerprint });
    await reload();
    if (done.rebase && done.rebase.ok) toast(`${c.label} rebased onto ${f.base}; grading it again.`, { timeout: 6000 });
    else toast(`The rebase stopped: ${(done.rebase && done.rebase.summary) || "see Review"}. Ask the agent to resolve it from its review drawer.`, { tone: "bad", timeout: 9000 });
  }

  // Remove (or stop) the losers: each with a box; unique work needs force.
  function cleanupWinner(f, winner) {
    return busy(f, null, "cleanup", async () => {
      const items = (await post(f, "cleanup", { candidates: [winner.label], dry_run: true })).items;
      await cleanupItems(f, items, `${winner.label}'s merged worktree`);
    });
  }

  function cleanup(f, preview) {
    return busy(f, null, "cleanup", async () => {
      const items = preview || (await post(f, "cleanup", { dry_run: true })).items;
      await cleanupItems(f, items, `The others of ${f.name}`);
    });
  }

  async function cleanupItems(f, items, title) {
    if (!items.length) {
      toast("Nothing to clean up.");
      return;
    }
    const boxes = new Map();
    const rows = items.map((i) => {
      const box = el("input", { type: "checkbox", checked: true });
      boxes.set(i.candidate, box);
      return el(
        "li",
        {},
        el("label", { className: "fo-pickrow" }, box, el("b", { textContent: i.candidate }), el("span", { className: "fo-path path-tail", textContent: pathTail(i.path), title: i.path })),
        el("span", { className: `fo-loser-state${i.refusal ? " keep" : ""}`, textContent: i.refusal ? `holds unique work: ${i.refusal}` : "nothing unique: removable" }),
      );
    });
    const unique = items.some((i) => i.refusal);
    const force = el("input", { type: "checkbox" });
    const go = await ask({
      kicker: "clean up",
      title,
      body: [
        el("ul", { className: "fo-losers" }, rows),
        unique
          ? el("label", { className: "fo-force" }, force, " remove even those holding unique work (their commits stay on their branches; uncommitted work is lost)")
          : null,
      ],
      buttons: [
        { label: "Stop sessions only", value: "stop" },
        { label: "Remove worktrees", value: "remove", tone: "danger" },
      ],
    });
    if (!go) return;
    const candidates = items.map((i) => i.candidate).filter((label) => boxes.get(label).checked);
    if (!candidates.length) return;
    const report = await post(f, "cleanup", {
      candidates,
      remove_worktrees: go === "remove",
      force: go === "remove" && force.checked,
    });
    const kept = report.items.filter((i) => !i.done);
    await reload();
    ctx.refresh();
    const carried = candidates.length - kept.length;
    if (kept.length) {
      // Said in a dialog of its own: the reasons matter, and the board
      // is modal.
      await ask({
        kicker: "clean up",
        title: `Kept ${kept.map((i) => i.candidate).join(", ")}`,
        body: [
          carried ? el("p", { className: "sheet-body", textContent: `${go === "remove" ? "Removed" : "Stopped"} ${carried}; these were kept:` }) : null,
          el("ul", { className: "fo-losers" }, kept.map((i) =>
            el("li", {}, el("b", { textContent: i.candidate }), el("span", { className: "fo-loser-state keep", textContent: i.error || i.refusal || "not removed" })))),
          el("p", { className: "fo-note", textContent: "Clean up again with \"remove even those holding unique work\" ticked to remove them anyway." }),
        ],
        buttons: [],
      });
    } else {
      toast(go === "remove" ? `Removed ${candidates.length} worktree${candidates.length === 1 ? "" : "s"}.` : `Stopped ${candidates.length} session${candidates.length === 1 ? "" : "s"}.`);
    }
  }

  async function forget(f) {
    const go = await ask({
      kicker: "forget",
      title: `Forget ${f.name}?`,
      body: [el("p", { className: "sheet-body", textContent: "The record and its board go; its sessions, worktrees, and branches stay as they are." })],
      buttons: [{ label: "Forget", value: "go", tone: "danger" }],
    });
    if (go !== "go") return;
    try {
      await api("DELETE", `/api/fanouts/${f.id}`);
    } catch (error) {
      toast(error.message, { tone: "bad" });
      return;
    }
    delete seen[f.id];
    saveSeen();
    ui.open = null;
    await reload();
    const next = ui.list[0];
    if (next) open(next.id);
    else board.close();
  }

  // --- the launch form ------------------------------------------------

  const kindAvailable = (kind) => {
    const kinds = state.kinds || [];
    const k = kinds.find((x) => x.kind === kind);
    return !kinds.length || !k || k.available;
  };

  function selectOf(options, value, label) {
    return el("select", { "aria-label": label }, options.map(([v, label]) => el("option", { value: v, textContent: label, selected: v === (value || "") })));
  }

  function candidateRow(kind, model) {
    const kindSelect = el(
      "select",
      { "aria-label": "Agent", className: "fo-kind" },
      ["claude", "codex"].map((k) => el("option", { value: k, textContent: k, selected: k === kind, disabled: !kindAvailable(k) })),
    );
    const listId = `fo-models-${Math.random().toString(36).slice(2, 8)}`;
    const models = el("datalist", { id: listId });
    const modelInput = el("input", { type: "text", value: model || "", placeholder: "model (CLI default)", spellcheck: false, autocomplete: "off", "aria-label": "Model" });
    // `list` is a read-only property: set as the attribute.
    modelInput.setAttribute("list", listId);
    const fill = () => models.replaceChildren(...(MODELS[kindSelect.value] || []).map((m) => el("option", { value: m })));
    kindSelect.addEventListener("change", fill);
    fill();
    const row = el("li", { className: "fo-cand-row" }, kindSelect, modelInput, models);
    row.append(el("button", {
      type: "button",
      className: "icon",
      title: "Remove this agent",
      "aria-label": "Remove this agent",
      onclick: () => {
        if (row.parentElement.children.length > MIN) row.remove();
        launchCount();
      },
    }, icon("x")));
    row.read = () => ({ kind: kindSelect.value, model: modelInput.value.trim() });
    return row;
  }

  function graderRow(name, run) {
    const nameInput = el("input", { type: "text", value: name || "", placeholder: "name", maxLength: 64, spellcheck: false, autocomplete: "off", "aria-label": "Grader name" });
    const runInput = el("input", { type: "text", value: run || "", placeholder: "cargo test -p crate   or   /abs/path/check.sh", spellcheck: false, autocomplete: "off", "aria-label": "Grader command" });
    const row = el("li", { className: "fo-grader-row" }, nameInput, runInput);
    row.append(el("button", {
      type: "button",
      className: "icon",
      title: "Remove this grader",
      "aria-label": "Remove this grader",
      onclick: () => {
        if (row.parentElement.children.length > 1) row.remove();
      },
    }, icon("x")));
    row.read = () => ({ name: nameInput.value.trim(), run: runInput.value.trim() });
    return row;
  }

  let form = null;

  function launchCount() {
    if (!form) return;
    const n = form.cands.children.length;
    form.submit.textContent = `Launch ${n} agents`;
    form.addCand.disabled = n >= MAX;
  }

  async function loadBases() {
    const folder = form.folder.value;
    if (!folder) return;
    try {
      const plan = await api("GET", `/api/worktrees/plan?folder=${encodeURIComponent(folder)}`);
      form.bases.replaceChildren(...(plan.branches || []).map((b) => el("option", { value: b })));
      form.base.placeholder = plan.default_base ? `default: ${plan.default_base}` : "base branch";
    } catch (_) {
      form.base.placeholder = "base branch";
    }
  }

  function fillFolders() {
    const workspace = workspaceById(form.workspace.value);
    const folders = workspace ? workspace.folders.filter((x) => x.git && !x.missing) : [];
    form.folder.replaceChildren(...folders.map((x) => el("option", { value: x.path, textContent: `${folderLabel(x)} — ${x.path}` })));
    loadBases();
  }

  // `seed`: a flywheel task (GET /api/flywheel/task) to fan out.
  function openLaunch(workspaceId, seed) {
    const usable = state.workspaces.filter((w) => !w.machine && w.folders.some((x) => x.git && !x.missing));
    if (!usable.length) {
      toast("A fan-out needs a workspace with a git folder: create one first.", { tone: "bad" });
      return;
    }
    if (board.open) board.close();
    const preferred = usable.find((w) => w.id === workspaceId) ||
      usable.find((w) => w.id === state.currentWorkspace) || usable[0];
    const workspace = el("select", { "aria-label": "Workspace" }, usable.map((w) => el("option", { value: w.id, textContent: w.name, selected: w === preferred })));
    const folder = el("select", { "aria-label": "Folder" });
    const bases = el("datalist", { id: "fo-bases" });
    const base = el("input", { type: "text", spellcheck: false, autocomplete: "off", placeholder: "base branch" });
    base.setAttribute("list", "fo-bases");
    const name = el("input", { type: "text", maxLength: 100, placeholder: "default: the prompt's first words", autocomplete: "off" });
    const prompt = el("textarea", { rows: 5, placeholder: "The task every agent gets, word for word", required: true });
    const cands = el("ol", { className: "fo-cands" });
    const defaults = kindAvailable("codex") ? [["claude", "opus"], ["claude", "sonnet"], ["codex", ""]] : [["claude", "opus"], ["claude", "sonnet"]];
    for (const [k, m] of defaults) cands.append(candidateRow(k, m));
    const addCand = el("button", { type: "button", className: "text-btn", onclick: () => {
      if (cands.children.length < MAX) cands.append(candidateRow("claude", ""));
      launchCount();
    } }, icon("plus"), "agent");
    const graders = el("ol", { className: "fo-grader-rows" },
      seed && seed.graders.length ? seed.graders.map((g) => graderRow(g.name || "", g.run)) : graderRow("", ""));
    const addGrader = el("button", { type: "button", className: "text-btn", onclick: () => graders.append(graderRow("", "")) }, icon("plus"), "grader");
    const mode = selectOf(CLAUDE_MODES, "", "Claude permission mode");
    const approval = selectOf(CODEX_APPROVALS, "", "Codex approval");
    const sandbox = selectOf(CODEX_SANDBOXES, "", "Codex sandbox");
    const timeout = el("input", { type: "number", min: 1, max: 14400, value: 900, "aria-label": "Grader timeout (seconds)" });
    const protect = el("textarea", { rows: 2, placeholder: "tests/\nscripts/check.sh", spellcheck: false });
    const commit = el("input", { type: "checkbox", checked: true });
    const error = el("p", { className: "fo-error", role: "alert", hidden: true });
    const submit = el("button", { type: "submit", className: "slab", textContent: "Launch" });
    const field = (label, hint, ...children) =>
      el("label", { className: "field" }, el("span", { className: "field-label" }, label, hint ? el("span", { className: "field-hint", textContent: ` ${hint}` }) : null), ...children);
    const group = (label, hint, ...children) =>
      el("div", { className: "field" }, el("span", { className: "field-label" }, label, hint ? el("span", { className: "field-hint", textContent: ` ${hint}` }) : null), ...children);
    const formNode = el(
      "form",
      { method: "dialog", novalidate: true },
      el("header", { className: "sheet-head" }, el("span", { className: "sheet-kicker", textContent: "verified fan-out" }), el("h2", { textContent: seed ? `Fan out flywheel task ${seed.task_id}` : "One prompt, several agents" })),
      el("p", { className: "sheet-body", textContent: "Each agent works in its own sibling worktree cut from the same commit. When one reports a finished turn the hub runs your graders in its worktree; the board ranks them: graders passing first, then the smaller diff, then the lower cost." }),
      el("div", { className: "fo-two" }, field("Workspace", "", workspace), field("Folder", "git", folder)),
      el("div", { className: "fo-two" }, field("Base branch", "every worktree starts here", base, bases), field("Name", "", name)),
      seed ? seedNote(seed) : null,
      field("Prompt", "", prompt),
      group("Agents", `${MIN}-${MAX}, a model each`, cands, addCand),
      group("Graders", "run by the hub with /bin/sh -c in each worktree; not added to the prompt (a task's prompt lists its visible graders, as the worker's does). A script named by absolute path is pinned (changed, it fails every grading); one inside the repo is protected.", graders, addGrader),
      el("details", { className: "fo-more" },
        el("summary", { textContent: "options: permissions, timeout, protected paths" }),
        el("div", { className: "fo-two" }, field("Claude permission mode", "", mode), field("Grader timeout", "seconds, each", timeout)),
        el("div", { className: "fo-two" }, field("Codex approval", "", approval), field("Codex sandbox", "", sandbox)),
        field("Protected paths", "one per line, relative to the repo; a candidate changing one fails", protect)),
      el("label", { className: "fo-check" }, commit, el("span", { textContent: "ask the agents to commit their work when done (the winner lands as commits)" })),
      error,
      el("footer", { className: "sheet-foot" }, el("button", { type: "button", className: "slab ghost", "data-close": true, textContent: "Cancel" }), submit),
    );
    form = { node: formNode, workspace, folder, base, bases, name, prompt, cands, addCand, graders, mode, approval, sandbox, timeout, protect, commit, error, submit, seed: seed || null };
    if (seed) {
      name.value = seed.task_id;
      prompt.value = seed.prompt;
      protect.value = (seed.protect || []).join("\n");
    }
    workspace.addEventListener("change", fillFolders);
    folder.addEventListener("change", loadBases);
    mode.addEventListener("change", () => {
      if (mode.value === "bypassPermissions") {
        error.hidden = false;
        error.textContent = "bypassPermissions: every Claude agent runs every tool (shell, edits, network) without asking.";
      } else {
        error.hidden = true;
      }
    });
    formNode.addEventListener("submit", (event) => {
      event.preventDefault();
      submitLaunch();
    });
    launch.replaceChildren(formNode);
    fillFolders();
    if (seed && [...folder.options].some((o) => o.value === seed.folder)) {
      folder.value = seed.folder;
      loadBases();
    }
    launchCount();
    launch.showModal();
    prompt.focus();
  }

  async function submitLaunch() {
    const f = form;
    const fail = (message) => {
      f.error.hidden = false;
      f.error.textContent = message;
    };
    const candidates = [...f.cands.children].map((row) => {
      const { kind, model } = row.read();
      const spec = { kind };
      if (model) spec.model = model;
      if (kind === "claude" && f.mode.value) spec.permission_mode = f.mode.value;
      if (kind === "codex" && f.approval.value) spec.approval = f.approval.value;
      if (kind === "codex" && f.sandbox.value) spec.sandbox = f.sandbox.value;
      return spec;
    });
    const graders = [...f.graders.children].map((row) => row.read()).filter((g) => g.run).map((g) => (g.name ? g : { run: g.run }));
    if (!f.prompt.value.trim()) return fail("Write the prompt every agent gets.");
    const held = f.seed ? f.seed.held_out.length : 0;
    if (!graders.length && !held) return fail("Add at least one grader command: it is what verifies the agents.");
    const body = {
      workspace_id: f.workspace.value,
      folder: f.folder.value || undefined,
      base: f.base.value.trim() || undefined,
      name: f.name.value.trim() || undefined,
      prompt: f.prompt.value,
      candidates,
      graders,
      timeout_secs: Number(f.timeout.value) || 900,
      protect: f.protect.value.split("\n").map((p) => p.trim()).filter(Boolean),
      commit_instruction: f.commit.checked,
      from_task: f.seed ? f.seed.task_id : undefined,
      ...ctx.termSize(),
    };
    f.submit.disabled = true;
    f.submit.textContent = "Launching…";
    let view;
    try {
      view = await api("POST", "/api/fanouts", body);
    } catch (error) {
      f.submit.disabled = false;
      launchCount();
      return fail(error.message);
    }
    launch.close();
    toast(`Fan-out ${view.name}: ${view.candidates.length} agents started.`);
    ctx.refresh();
    await reload();
    open(view.id);
  }

  // What a task-seeded launch adds and leaves out.
  function seedNote(seed) {
    const held = seed.held_out || [];
    return el("div", { className: "fo-seed" },
      el("p", { textContent: `From ${seed.phase ? `${seed.phase}/` : ""}${seed.task_id}: its goal and context as the prompt, its command graders below (edit either).` }),
      held.length
        ? el("p", { className: "fo-seed-held", textContent: `+ ${held.length} held-out grader${held.length === 1 ? "" : "s"} (${held.join(", ")}): added by the hub at launch, run in every worktree, never shown to the agents.` })
        : null,
      (seed.not_run || []).length
        ? el("p", { textContent: `Not run by the hub (the worker still applies them): ${seed.not_run.join(", ")}.` })
        : null,
      (seed.protect_skipped || []).length
        ? el("p", { textContent: `Protected at landing only (globs the hub cannot hold a candidate to): ${seed.protect_skipped.join(", ")}.` })
        : null);
  }

  function paletteItems(cmd) {
    cmd("New verified fan-out", "one prompt, 2-6 agents in worktrees, graded by the hub", () => openLaunch(), "plus");
    for (const f of ui.list) cmd(`Fan-out: ${f.name}`, headline(f), () => open(f.id), "branch");
  }

  // Ages tick while the board shows.
  setInterval(() => {
    if (board.open && document.visibilityState === "visible" && !act.open) renderBoard();
  }, 30000);

  return {
    reload,
    onEvent,
    attention,
    attentionRow,
    candidateSessions,
    open,
    openLaunch,
    paletteItems,
    isOpen: () => board.open,
    close: () => {
      if (board.open) board.close();
    },
  };
};

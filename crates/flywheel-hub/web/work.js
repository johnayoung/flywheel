// flywheel hub: Work -- what one agent session is working on, from its
// telemetry (crates/flywheel-hub/src/telemetry). A drawer over the
// terminal for a session this browser can attach (the header's work pill,
// the palette, a fleet row); the same content in a read-only sheet for a
// session a collector holds only as telemetry (fleet.js).
//
// .../turns folds each of the newest turns: a strip of its calls along its
// own clock (coloured by kind of work: files, edits, shell, wait, network,
// agents...), a one-line digest (files read and edited, commands run and
// how they failed, cost), and how it ended. The drawer opens over the
// terminal and never replaces it.
// GET /api/telemetry/sessions/{id} gives the session in full: its status,
// context, cost, rate limits, the agent's declared plan, and the tree of
// turns and subagents. .../events gives the event log the tool calls are
// read from: the newest page on open, what came after on each `telemetry`
// push for the session, older ones only when asked. Nothing here is
// inferred: a plan is the agent's own declaration and is labelled so; a
// session without one shows counts, never a percentage.
// app-base.js creates it with its helpers; fleet.js shares its `parts`.
"use strict";

window.FwHubWork = function (ctx) {
  const { $, el, icon, rawApi, span, state, sessionById, workspaceById, isAgent } = ctx;
  const { tokens, dollars, plural } = window.FwHubFmt;
  // The live map above the tree (map.js), one for whichever view is open.
  const map = window.FwHubMap({
    // Back from a replay: the session as it is, again.
    onLive: () => map.feed(view.events, false),
    onReplayLast: () => {
      const turns = ((view.detail && view.detail.work) || []).filter((w) => !w.parent);
      const newest = turns[turns.length - 1];
      if (newest) replayTurn(newest.id);
    },
  });
  let mapShown = ctx.store.get("work-map", true) !== false;

  const PAGE = 300;
  const TURNS = 30;
  const OLDER_PAGE = 1000;

  // --- shared parts (fleet.js draws its rows with these) -------------------

  // The hub's clock minus this browser's: durations tick on the hub's.
  let skew = 0;
  const now = () => Date.now() + skew;
  function setClock(serverNow) {
    if (Number.isFinite(serverNow)) skew = serverNow - Date.now();
  }

  // 850ms, 5.4s, 42s, 3m 12s, 2h 5m; `whole`: whole seconds (a ticking one).
  function dur(ms, whole) {
    ms = Math.max(0, ms);
    if (ms < 1000 && !whole) return `${Math.round(ms)}ms`;
    const s = ms / 1000;
    if (s < 60) return whole || s >= 10 ? `${Math.floor(s)}s` : `${s.toFixed(1)}s`;
    if (s < 3600) return `${Math.floor(s / 60)}m ${Math.floor(s % 60)}s`;
    if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
    return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
  }

  // A duration that keeps counting from `since` (unix ms), on top of
  // `base` ms already closed; `coarse`: the UI's 42s / 5m / 3h.
  function live(since, base, coarse) {
    const node = el("span", {
      className: "wk-live",
      dataset: { since: String(since), base: String(base || 0), coarse: coarse ? "1" : "" },
    });
    paintLive(node);
    return node;
  }
  function paintLive(node) {
    const ms = Number(node.dataset.base) + Math.max(0, now() - Number(node.dataset.since));
    const text = node.dataset.coarse ? span(ms / 1000) : dur(ms, true);
    if (node.textContent !== text) node.textContent = text;
  }
  function tick(root) {
    for (const node of root.querySelectorAll(".wk-live")) paintLive(node);
  }

  // A telemetry status in the sidebar's dot vocabulary.
  const LOOK = {
    ...ctx.agentLook,
    failed: { dot: "bad", label: "failed", tone: "bad" },
    ended: { dot: "stopped", label: "ended", tone: "" },
  };
  const lookOf = (status) => LOOK[status] || { dot: "stopped", label: status || "unknown", tone: "" };

  const ASKS = { answer: "a reply", decide: "a decision", run: "a command run" };
  // What a blocked session waits on: "permission: Bash", "question"; an
  // asking one: "a decision".
  function blockedOn(snap) {
    if (snap.status === "asking") return (snap.ask && ASKS[snap.ask.kind]) || "a reply";
    if (snap.status !== "blocked") return "";
    if (snap.blocked_on === "permission") return `permission${snap.blocked_tool ? `: ${snap.blocked_tool}` : ""}`;
    return snap.blocked_on || "a person";
  }

  // The dot and the status word; blocked: on what, and for how long.
  function statusNode(snap) {
    const look = lookOf(snap.status);
    const on = blockedOn(snap);
    return el(
      "span",
      { className: `wk-status ${snap.status || ""}` },
      el("span", { className: "wk-status-word" }, el("span", { className: `dot ${look.dot}` }), el("b", { textContent: look.label })),
      on ? el("span", { className: "wk-status-on" }, `${on} `, live(snap.since, 0, true)) : null,
    );
  }

  // The existing progress bar at `pct` percent.
  function bar(pct, hot) {
    const width = Math.max(0, Math.min(100, Number(pct) || 0));
    return el("span", { className: `prog-bar wk-bar${hot ? " hot" : ""}` }, el("i", { style: `width:${width}%` }));
  }

  const contextTitle = (c) => `${tokens(c.used)} of ${tokens(c.window)} tokens${c.exact === false ? " (an estimate)" : ""}`;
  const contextPct = (c) => `${c.exact === false ? "~" : ""}${Math.round(c.pct)}%`;

  // The context meter: the bar, the percentage (an estimate led by "~").
  function contextNode(c) {
    const hot = c.pct >= 85;
    return el("span", { className: `wk-ctx${hot ? " hot" : ""}`, title: contextTitle(c) }, bar(c.pct, hot), el("span", { className: "wk-num", textContent: contextPct(c) }));
  }

  const usageTitle = (u) =>
    `${tokens(u.input)} in, ${tokens(u.output)} out, ${tokens(u.cache_read)} cache read, ${tokens(u.cache_write)} cache write`;

  const WINDOW = { five_hour: "5h", seven_day: "7d" };
  const windowLabel = (name) => WINDOW[name] || String(name || "").replace(/_/g, " ");
  // "resets in 3h" ("" once past, or unknown); `resets_at` is unix seconds.
  function resetsIn(rate) {
    if (!rate.resets_at) return "";
    const left = rate.resets_at - now() / 1000;
    return left > 0 ? `resets in ${span(left)}` : "";
  }
  const rateText = (rate) => `${windowLabel(rate.window)} ${Math.round(rate.used_pct)}%`;

  const shortHost = (host) => String(host || "").slice(0, 8);

  const parts = {
    now, setClock, dur, live, tick, lookOf, blockedOn, statusNode, bar,
    contextNode, usageTitle, rateText, resetsIn, shortHost,
  };

  // --- state ---------------------------------------------------------------

  const drawer = $("#work");
  const dialog = $("#dlg-work");
  const view = {
    target: null, // { id, host, machine, mode: "drawer" | "dialog" }
    detail: null,
    missing: false, // the hub has nothing for the session yet
    error: "",
    events: [], // oldest first
    loaded: false, // the newest page arrived
    complete: false, // nothing older is left to load
    older: false, // older events are being loaded
    after: false, // newer events are being loaded
    again: false, // ...and a push came meanwhile
    expanded: new Map(), // turn id -> whether its calls show (the user's choice)
    turns: new Map(), // turn id -> its digest (GET .../turns)
    seq: 0,
    abort: null,
    timer: 0,
    detailTimer: 0,
    detailAt: 0,
  };

  const lastSeq = () => (view.events.length ? view.events[view.events.length - 1].seq : 0);
  // Events the detail knows of and the log held here does not: fetch them.
  function catchUp() {
    if (view.loaded && view.detail && view.detail.seq > lastSeq()) loadAfter();
  }

  const bodyOf = (target) => (target.mode === "drawer" ? $("#wk-body") : $("#wkd-body"));

  function path(target, tail, params) {
    const p = new URLSearchParams(params || {});
    if (target.host) p.set("host", target.host);
    const query = p.toString();
    const prefix = target.machine ? ctx.machinePrefix(target.machine) : "";
    return `${prefix}/api/telemetry/sessions/${encodeURIComponent(target.id)}${tail}${query ? `?${query}` : ""}`;
  }

  // --- fetching ------------------------------------------------------------

  async function loadDetail() {
    const target = view.target;
    const seq = view.seq;
    if (!target) return;
    try {
      const data = await rawApi("GET", path(target, ""), undefined, view.abort.signal);
      if (seq !== view.seq) return;
      setClock(data.now);
      view.detail = data.session;
      view.missing = false;
      view.error = "";
      map.detail(view.detail);
      catchUp();
      loadTurns();
    } catch (error) {
      if (seq !== view.seq) return;
      if (error.status === 404) {
        view.missing = true;
        view.detail = null;
        view.error = "";
      } else {
        view.error = error.message;
      }
    }
    render();
  }

  // The newest turns' digests (with each detail read: at most once a second).
  async function loadTurns() {
    const target = view.target;
    const seq = view.seq;
    try {
      const data = await rawApi("GET", path(target, "/turns", { limit: String(TURNS) }), undefined, view.abort.signal);
      if (seq !== view.seq) return;
      view.turns = new Map((data.turns || []).map((t) => [t.id, t]));
      render();
    } catch (error) {
      // The digests only add to the tree: without them it shows as before.
      if (seq !== view.seq || error.name === "AbortError") return;
    }
  }

  async function loadNewest() {
    const target = view.target;
    const seq = view.seq;
    try {
      const data = await rawApi("GET", path(target, "/events", { newest: "1", limit: String(PAGE) }), undefined, view.abort.signal);
      if (seq !== view.seq) return;
      view.events = data.events || [];
      view.complete = view.events.length < PAGE || view.events[0].seq <= 1;
      view.loaded = true;
      map.feed(view.events, false);
      catchUp();
    } catch (error) {
      if (seq !== view.seq) return;
      view.error = error.message;
    }
    render();
  }

  // What came after the newest event held (a push said there is more).
  async function loadAfter() {
    if (view.after) {
      view.again = true;
      return;
    }
    const target = view.target;
    const seq = view.seq;
    view.after = true;
    try {
      for (;;) {
        const last = lastSeq();
        const data = await rawApi("GET", path(target, "/events", { after: String(last), limit: String(PAGE) }), undefined, view.abort.signal);
        if (seq !== view.seq) return;
        const fresh = (data.events || []).filter((e) => e.seq > last);
        view.events = view.events.concat(fresh);
        map.feed(fresh, last > 0);
        if (last === 0) view.complete = true;
        if (fresh.length < PAGE && !view.again) break;
        view.again = false;
      }
    } catch (error) {
      if (seq !== view.seq) return;
      view.error = error.message;
    } finally {
      if (seq === view.seq) {
        view.after = false;
        view.again = false;
      }
    }
    render();
  }

  // Everything before the oldest event held: paged from the start (the
  // log has no "before"), only when asked.
  async function loadOlder() {
    if (view.older || view.complete || !view.events.length) return;
    const target = view.target;
    const seq = view.seq;
    const first = view.events[0].seq;
    view.older = true;
    render();
    const older = [];
    try {
      let after = 0;
      for (;;) {
        const data = await rawApi("GET", path(target, "/events", { after: String(after), limit: String(OLDER_PAGE) }), undefined, view.abort.signal);
        if (seq !== view.seq) return;
        const page = data.events || [];
        for (const event of page) {
          if (event.seq < first) older.push(event);
        }
        if (page.length < OLDER_PAGE || page[page.length - 1].seq >= first - 1) break;
        after = page[page.length - 1].seq;
      }
      view.events = older.concat(view.events);
      view.complete = true;
    } catch (error) {
      if (seq !== view.seq) return;
      view.error = error.message;
    } finally {
      if (seq === view.seq) view.older = false;
    }
    render();
  }

  // The detail again, at most once a second.
  function detailSoon() {
    if (view.detailTimer) return;
    const wait = Math.max(0, 1000 - (Date.now() - view.detailAt));
    view.detailTimer = setTimeout(() => {
      view.detailTimer = 0;
      view.detailAt = Date.now();
      loadDetail();
    }, wait);
  }

  // A `telemetry` push (this hub's, or `machine`'s): the open session's
  // newer events and its detail follow.
  function onSnapshot(snap, machine) {
    const target = view.target;
    if (!target || !snap || typeof snap !== "object") return;
    if (snap.session !== target.id || (machine || null) !== (target.machine || null)) return;
    if (target.host ? snap.host !== target.host : view.detail && snap.host !== view.detail.host) return;
    if (view.missing) {
      view.detailAt = Date.now();
      loadDetail();
      loadNewest();
      return;
    }
    if (view.detail && snap.seq >= view.detail.seq) {
      // The strip follows at once; the tree and plans with the refetch.
      view.detail = { ...snap, work: view.detail.work, plans: view.detail.plans };
      map.detail(view.detail);
      render();
    }
    if (view.loaded && snap.seq > lastSeq()) loadAfter();
    detailSoon();
  }

  // --- rendering: the summary strip ----------------------------------------

  function fact(label, ...value) {
    return [el("dt", { textContent: label }), el("dd", {}, ...value)];
  }

  function summary(d) {
    const rows = [fact("status", statusNode(d))];
    // What an asking session asks, in full (the sidebar's Needs you
    // answers it).
    const linked = (label, id) => {
      if (!id) return;
      const known = sessionById(id);
      rows.push(fact(
        label,
        known
          ? el("button", { type: "button", className: "tach wk-link", textContent: known.name, title: "Open it", onclick: () => ctx.openSession(known.id) })
          : el("span", { className: "wk-mono", textContent: id.slice(0, 8), title: id }),
      ));
    };
    linked("continues", d.links && d.links.continues);
    linked("opened by", d.links && d.links.opened_by);
    linked("continued", d.continued_by);
    if (d.ask && d.ask.prompt) {
      rows.push(fact("handoff", el("pre", { className: "wk-ask-cmd wk-handoff", textContent: d.ask.prompt })));
    }
    if (d.ask) {
      rows.push(fact(
        "asks",
        el("span", { className: "wk-ask", textContent: d.ask.text }),
        d.ask.command ? el("pre", { className: "wk-ask-cmd wk-mono", textContent: d.ask.command }) : null,
        (d.ask.options || []).length ? el("span", { className: "wk-dim", textContent: ` options: ${d.ask.options.join(" · ")}` }) : null,
        el("span", { className: "wk-dim", textContent: d.ask.source === "declared" ? " (declared)" : " (its closing question)" }),
      ));
    }
    if (d.context) rows.push(fact("context", contextNode(d.context), el("span", { className: "wk-dim", textContent: ` ${contextTitle(d.context)}` })));
    if (d.usage) {
      rows.push(fact(
        "cost",
        el("b", { className: "wk-num", textContent: d.usage.cost_usd == null ? "-" : dollars(d.usage.cost_usd) }),
        el("span", { className: "wk-dim", textContent: ` ${usageTitle(d.usage)}` }),
      ));
    }
    const rates = d.rate || [];
    if (rates.length || d.limited_at) {
      const bits = rates.map((rate) => {
        const resets = resetsIn(rate);
        return `${rateText(rate)}${resets ? `, ${resets}` : ""}`;
      });
      rows.push(fact(
        "limits",
        bits.join(" · "),
        d.limited_at ? el("span", { className: "wk-bad", textContent: `${bits.length ? " · " : ""}refused ${span((now() - d.limited_at) / 1000)} ago` }) : null,
      ));
    }
    const blocked = d.status === "blocked";
    if (d.blocked_ms || d.waits || blocked) {
      rows.push(fact(
        "blocked",
        blocked ? live(d.since, d.blocked_ms || 0) : dur(d.blocked_ms || 0, true),
        el("span", { className: "wk-dim", textContent: ` on a person${d.waits ? ` · ${plural(d.waits, "wait")}` : ""}` }),
      ));
    }
    if (d.awaiting_ms) rows.push(fact("idle", dur(d.awaiting_ms, true), el("span", { className: "wk-dim", textContent: " awaiting the next prompt" })));
    rows.push(fact(
      "tools",
      plural(d.tools || 0, "call"),
      d.tool_errors ? el("span", { className: "wk-bad", textContent: ` · ${plural(d.tool_errors, "error")}` }) : null,
      d.denied ? el("span", { className: "wk-bad", textContent: ` · ${d.denied} denied` }) : null,
    ));
    if (d.compactions) rows.push(fact("compact", plural(d.compactions, "compaction")));
    if (d.last_error) rows.push(fact("error", el("span", { className: "wk-bad wk-mono", textContent: d.last_error })));
    return el("dl", { className: "facts-grid wk-facts" }, rows.flat());
  }

  // --- rendering: plans ----------------------------------------------------

  function mark(status) {
    const done = status === "done" || status === "ok";
    return el("span", { className: `wk-mark ${status}`, title: status }, done ? icon("check") : null);
  }

  function planItem(item) {
    const bits = [];
    if (item.tools) bits.push(plural(item.tools, "call"));
    let time = null;
    if (item.started && item.finished) time = dur(item.finished - item.started, true);
    else if (item.started && item.status === "active") time = live(item.started, 0);
    return el(
      "li",
      { className: `wk-item ${item.status}` },
      mark(item.status),
      el("span", { className: "wk-item-title", textContent: item.title }),
      el("span", { className: "wk-meta" }, bits.join(" · "), bits.length && time ? " · " : "", time),
    );
  }

  const planDone = (plan) => plan.items.filter((i) => i.status === "done").length;
  // Cancelled items are no longer the plan's to do.
  const planTotal = (plan) => plan.items.filter((i) => i.status !== "cancelled").length;

  function planRev(plan) {
    if (!(plan.revision > 1)) return null;
    const delta = [plan.added ? `+${plan.added}` : "", plan.dropped ? `-${plan.dropped}` : ""].filter(Boolean).join(" ");
    return el("span", { className: "wk-rev", textContent: `rev ${plan.revision}${delta ? ` (${delta})` : ""}`, title: "Times the agent rewrote this plan (items added, dropped)" });
  }

  function planSection(plan) {
    const done = planDone(plan);
    const total = planTotal(plan);
    return el(
      "section",
      { className: "fw-sec wk-plan" },
      el(
        "h3",
        {},
        "Plan",
        el("span", { className: "fw-n", textContent: `${done}/${total}` }),
        el("span", { className: "wk-tag", textContent: plan.source }),
        planRev(plan),
      ),
      el("div", { className: "wk-plan-bar" }, bar(total ? (done / total) * 100 : 0)),
      el("ol", { className: "wk-items" }, plan.items.map(planItem)),
      plan.note ? el("p", { className: "fw-note wk-note", textContent: plan.note }) : null,
      el("p", { className: "wk-cap", textContent: "As declared by the agent: its own account, not verified by the hub." }),
    );
  }

  // A subagent's plan, under it in the tree.
  function planCompact(plan) {
    return el(
      "div",
      { className: "wk-subplan" },
      el(
        "div",
        { className: "wk-subplan-head" },
        el("span", { className: "wk-tag", textContent: plan.source }),
        el("span", { className: "fw-n", textContent: `${planDone(plan)}/${planTotal(plan)}` }),
        planRev(plan),
      ),
      el("ol", { className: "wk-items compact" }, plan.items.map(planItem)),
    );
  }

  // --- rendering: the tree -------------------------------------------------

  // "9 calls . 1 error" (the errors in the alert tone), then the time.
  function tally(item, extra) {
    return el(
      "span",
      { className: "wk-meta" },
      plural(item.tools || 0, "call"),
      item.tool_errors ? el("span", { className: "wk-bad", textContent: ` · ${plural(item.tool_errors, "error")}` }) : null,
      extra ? ` · ${extra}` : "",
      " · ",
      elapsed(item),
    );
  }

  const statusTag = (status) => el("span", { className: `wk-st ${status}`, textContent: status });

  function elapsed(item) {
    if (item.finished) return dur(item.finished - item.started, true);
    return item.status === "active" ? live(item.started, 0) : null;
  }

  // --- rendering: a turn's digest ------------------------------------------

  const STATION_LABEL = {
    files: "reading", edits: "editing", shell: "shell", wait: "waiting", network: "network",
    browser: "browser", agents: "agents", skills: "skills", plan: "plan", memory: "memory",
    you: "asking you", other: "other",
  };

  // The turn's calls along its own clock: where each started and how long
  // it took, coloured by kind of work, failures marked.
  function strip(t) {
    // A subagent can outlast its turn: the strip runs to its last call.
    const last = t.strip.reduce((n, seg) => Math.max(n, t.started + seg.at + (seg.ms || 0)), 0);
    const end = Math.max(t.finished || now(), last);
    const total = Math.max(1, end - t.started);
    const pct = (ms) => Math.min(100, Math.max(0, (ms / total) * 100));
    return el("div", { className: "wk-strip", role: "img", "aria-label": `Calls of this turn along its ${dur(total, true)}` },
      t.strip.map((seg) => {
        const ms = seg.ms == null ? Math.max(0, end - t.started - seg.at) : seg.ms;
        const left = pct(seg.at);
        const what = `${seg.what}${seg.calls > 1 ? ` (+${seg.calls - 1} more)` : ""}`;
        return el("span", {
          className: `wk-seg st-${seg.station}${seg.failed ? " failed" : ""}${seg.sub ? " sub" : ""}${seg.ms == null ? " running" : ""}`,
          style: `left:${left}%;width:max(2px,${Math.min(100 - left, pct(ms))}%)`,
          title: `${STATION_LABEL[seg.station] || seg.station} ${seg.ms == null ? "(running)" : dur(seg.ms)} at +${dur(seg.at, true)}\n${what}${seg.failed ? "\nfailed" : ""}`,
        });
      }));
  }

  const baseName = (path) => path.split("/").filter(Boolean).pop() || path;

  // One line: time by kind of work, files, commands, cost.
  function digestLine(t) {
    const parts = [];
    for (const st of t.stations.slice(0, 4)) {
      if (st.ms < 1000) continue;
      parts.push(el("span", { className: `wk-dg st-${st.station}${st.errors ? " bad" : ""}`, title: `${plural(st.calls, "call")}${st.errors ? `, ${st.errors} failed` : ""}` },
        el("i", { className: "wk-dot" }), `${STATION_LABEL[st.station] || st.station} ${dur(st.ms, true)}`));
    }
    if (t.read || t.edited) {
      const names = t.edits.slice(0, 3).map((f) => `${baseName(f.path)}${f.edits > 1 ? ` x${f.edits}` : ""}`);
      parts.push(el("span", { className: "wk-dg", title: t.edits.map((f) => `${f.path} (${plural(f.edits, "edit")})`).join("\n") },
        `read ${t.read}`, t.edited ? `, edited ${t.edited}${names.length ? `: ${names.join(", ")}${t.edited > names.length ? ", ..." : ""}` : ""}` : ""));
    }
    if (t.commands.length) {
      const top = t.commands.slice(0, 3).map((c) => `${c.shape}${c.calls > 1 ? ` x${c.calls}` : ""}${c.errors ? ` (${c.errors} failed)` : ""}`);
      parts.push(el("span", { className: `wk-dg${t.commands.some((c) => c.errors) ? " bad" : ""}`, title: t.commands.map((c) => `${c.shape}: ${plural(c.calls, "call")}, ${dur(c.ms, true)}${c.errors ? `, ${c.errors} failed` : ""}`).join("\n") },
        `ran ${top.join(", ")}${t.commands.length > 3 ? ", ..." : ""}`));
    }
    if (t.cost_usd != null) parts.push(el("span", { className: "wk-dg", textContent: dollars(t.cost_usd) }));
    if (t.compacted) parts.push(el("span", { className: "wk-dg warn", textContent: "compacted" }));
    if (t.waits) parts.push(el("span", { className: "wk-dg warn", textContent: `waited on you ${t.waits > 1 ? `${t.waits}x` : ""}`.trim() }));
    if (t.partial) parts.push(el("span", { className: "wk-dg quiet", textContent: "earlier calls not read" }));
    if (t.calls) {
      parts.push(el("button", {
        type: "button",
        className: "wk-replay",
        textContent: "replay",
        title: "Play this turn back on the map",
        dataset: { wk: `replay:${t.id}` },
        onclick: () => replayTurn(t.id),
      }));
    }
    return parts.length ? el("div", { className: "wk-digest" }, parts) : null;
  }

  // Its last line of prose: what the agent left the operator with.
  function endingLine(text) {
    const lines = text.split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("|") && !l.startsWith("```"));
    return lines.length ? lines[lines.length - 1] : "";
  }

  function digest(t, expanded) {
    if (!t) return null;
    const out = [];
    if (t.strip.length) out.push(strip(t));
    out.push(digestLine(t));
    if (t.error) out.push(el("p", { className: "wk-end bad", textContent: t.error }));
    else if (t.ending) {
      out.push(expanded
        ? el("pre", { className: "wk-end full", textContent: t.ending })
        : el("p", { className: "wk-end", textContent: endingLine(t.ending), title: "How the turn ended (open the turn for more)" }));
    }
    if (expanded && t.failure) {
      out.push(el("p", { className: "wk-end bad" }, `last failure: ${t.failure.tool}${t.failure.summary ? ` ${t.failure.summary}` : ""}${t.failure.error ? ` -- ${t.failure.error}` : ""}`));
    }
    return el("div", { className: "wk-turn-digest" }, out.filter(Boolean));
  }

  // A prompt another agent sent (Claude Code wraps it in an
  // <agent-message from="..."> tag) reads as such.
  function promptTitle(title) {
    const m = /^<agent-message[^>]*>\s*(?:\[([^\]]+)\]\s*)?/i.exec(title || "");
    if (!m) return title;
    const rest = title.slice(m[0].length).replace(/<\/agent-message>\s*$/i, "");
    return `${m[1] ? m[1].toLowerCase() : "agent message"}: ${rest}`;
  }

  // Turns newest first, each with its subagents and (expanded) its calls.
  function tree(d) {
    const work = d.work || [];
    const byId = new Map(work.map((item) => [item.id, item]));
    const kids = new Map();
    for (const item of work) {
      if (!item.parent) continue;
      if (!kids.has(item.parent)) kids.set(item.parent, []);
      kids.get(item.parent).push(item);
    }
    const rootOf = (id) => {
      let at = byId.get(id);
      for (let hops = 0; at && at.parent && hops < 64; hops++) at = byId.get(at.parent);
      return at ? at.id : null;
    };
    const byAgent = new Map(work.filter((item) => item.agent).map((item) => [item.agent, item]));
    const plans = new Map((d.plans || []).filter((p) => p.owner).map((p) => [p.owner, p]));

    // The lines each turn shows, in time order: tool calls (started and
    // finished paired by id) and the quiet markers.
    const lines = new Map();
    const open = new Map();
    const turnsOf = work.filter((item) => !item.parent);
    // An event that names no work item (a compaction, a rate-limit
    // refusal) goes to the turn that was running when it happened.
    const turnAt = (ts) => {
      const at = turnsOf.find((item, i) => ts >= item.started && (item.finished ? ts <= item.finished : i === turnsOf.length - 1 || ts < turnsOf[i + 1].started));
      return at ? at.id : "";
    };
    const push = (event, line) => {
      const root = event.work ? rootOf(event.work) || "" : turnAt(event.ts);
      if (!lines.has(root)) lines.set(root, []);
      lines.get(root).push(line);
      return line;
    };
    for (const event of view.events) {
      const body = event.body || {};
      if (event.kind === "tool.started") {
        open.set(body.id, push(event, { call: true, name: body.name, summary: body.summary || "", started: event.ts, agent: event.agent, work: event.work }));
      } else if (event.kind === "tool.finished") {
        // Its start is older than the events held: the finish alone.
        const call = open.get(body.id) || push(event, { call: true, name: body.name, summary: "", agent: event.agent, work: event.work });
        open.delete(body.id);
        call.outcome = body.outcome || "ok";
        call.error = body.error || "";
        call.ms = Number.isFinite(body.duration_ms) ? body.duration_ms : call.started ? event.ts - call.started : null;
      } else if (event.kind === "wait.started") {
        push(event, { note: `waiting on ${body.reason === "permission" ? `permission${body.tool ? `: ${body.tool}` : ""}` : `a ${body.reason || "person"}`}`, tone: "wait", ts: event.ts });
      } else if (event.kind === "compact") {
        push(event, { note: `${body.phase === "post" ? "context compacted" : "compacting context"}${body.trigger ? ` (${body.trigger})` : ""}`, tone: "", ts: event.ts });
      } else if (event.kind === "rate.limited") {
        push(event, { note: `rate limit refused${body.detail ? `: ${body.detail}` : ""}`, tone: "bad", ts: event.ts });
      } else if (event.kind === "turn.failed") {
        push(event, { note: `turn failed: ${body.error || "unknown error"}${body.detail ? ` -- ${body.detail}` : ""}`, tone: "bad", ts: event.ts });
      }
    }

    const running = d.status === "working" || d.status === "blocked";
    function line(entry) {
      if (!entry.call) {
        return el(
          "li",
          { className: `wk-note-line ${entry.tone}` },
          el("time", { className: "wk-at", textContent: new Date(entry.ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }), title: new Date(entry.ts).toLocaleString() }),
          el("span", { textContent: entry.note }),
        );
      }
      const owner = byId.get(entry.work);
      const pending = !entry.outcome;
      const ticking = pending && running && owner && owner.status === "active" && entry.started;
      const outcome = entry.outcome || (ticking ? "running" : "unfinished");
      const sub = entry.agent ? byAgent.get(entry.agent) : null;
      return el(
        "li",
        { className: `wk-call ${outcome}` },
        ticking ? el("span", { className: "wk-mark running" }, el("span", { className: "dot working" })) : mark(outcome),
        el("b", { className: "wk-call-name", textContent: entry.name || "tool" }),
        el("span", { className: "wk-call-sum", textContent: entry.summary, title: entry.summary }),
        entry.agent ? el("span", { className: "wk-tag", textContent: sub ? sub.kind || "subagent" : "subagent", title: sub ? `In the subagent: ${sub.title}` : `In subagent ${entry.agent}` }) : null,
        outcome !== "ok" && outcome !== "running" ? el("span", { className: `wk-st ${outcome}`, textContent: outcome }) : null,
        el("span", { className: "wk-meta wk-num" }, ticking ? live(entry.started, 0) : entry.ms == null ? "" : dur(entry.ms)),
        entry.error ? el("span", { className: "wk-call-err", textContent: entry.error }) : null,
      );
    }

    function subagent(item) {
      const plan = item.agent ? plans.get(item.agent) : null;
      const below = kids.get(item.id) || [];
      return el(
        "li",
        { className: `wk-sub ${item.status}` },
        el(
          "div",
          { className: "wk-sub-head" },
          item.kind ? el("span", { className: "wk-tag", textContent: item.kind }) : null,
          el("span", { className: "wk-sub-title", textContent: item.title || "(subagent)", title: item.title || "" }),
          statusTag(item.status),
          tally(item, item.tokens ? `${tokens(item.tokens)} tokens` : ""),
        ),
        item.status === "active" && item.doing ? el("div", { className: "wk-doing", textContent: item.doing, title: item.doing }) : null,
        plan ? planCompact(plan) : null,
        below.length ? el("ul", { className: "wk-subs" }, below.map(subagent)) : null,
      );
    }

    function turn(item, newest) {
      const chosen = view.expanded.get(item.id);
      const expanded = chosen === undefined ? newest : chosen;
      const below = kids.get(item.id) || [];
      const mine = lines.get(item.id) || [];
      const head = el(
        "button",
        {
          type: "button",
          className: "wk-turn-head",
          "aria-expanded": expanded ? "true" : "false",
          dataset: { wk: `turn:${item.id}` },
          onclick: () => {
            view.expanded.set(item.id, !expanded);
            render();
          },
        },
        icon("chev"),
        el("span", { className: `wk-turn-title${item.title ? "" : " none"}`, textContent: promptTitle(item.title) || "(prompt not seen)", title: item.title || "" }),
        statusTag(item.status),
        tally(item, ""),
      );
      let calls = null;
      if (expanded) {
        const held = mine.filter((entry) => entry.call).length;
        if (mine.length) {
          calls = [
            !view.complete && held < (item.tools || 0) ? el("p", { className: "wk-quiet", textContent: "Its earlier calls are older than the events loaded." }) : null,
            el("ol", { className: "wk-calls" }, mine.map(line)),
          ];
        } else if (item.tools && !view.complete) calls = el("p", { className: "wk-quiet", textContent: "Its calls are older than the events loaded." });
        else calls = el("p", { className: "wk-quiet", textContent: "No tool calls." });
      }
      return el(
        "li",
        { className: `wk-turn ${item.status}${expanded ? " open" : ""}` },
        head,
        digest(view.turns.get(item.id), expanded),
        below.length ? el("ul", { className: "wk-subs" }, below.map(subagent)) : null,
        calls,
      );
    }

    const turns = turnsOf.slice().reverse();
    const stray = lines.get("") || [];
    return el(
      "section",
      { className: "fw-sec wk-work" },
      el("h3", {}, "Work", el("span", { className: "fw-n", textContent: plural(turns.length, "turn") })),
      turns.length
        ? el("ol", { className: "wk-turns" }, turns.map((item, i) => turn(item, i === 0)))
        : el("p", { className: "wk-quiet", textContent: "No turn yet." }),
      stray.length
        ? el("div", { className: "wk-stray" }, el("span", { className: "wk-cap", textContent: "Outside a turn" }), el("ol", { className: "wk-calls" }, stray.map(line)))
        : null,
      view.complete
        ? null
        : el("div", { className: "act-more" }, el("button", {
          type: "button",
          className: "slab ghost",
          textContent: view.older ? "Loading..." : "Load older",
          disabled: view.older,
          title: "The tool calls of earlier turns",
          dataset: { wk: "older" },
          onclick: () => loadOlder(),
        })),
    );
  }

  // --- rendering: the frame ------------------------------------------------

  function renderHead() {
    const target = view.target;
    const d = view.detail;
    const session = target.mode === "drawer" ? sessionById(target.id) : null;
    const workspace = session && workspaceById(session.workspace_id);
    const title = session ? session.name : (d && d.label) || target.label || target.id.slice(0, 8);
    const harness = (d && d.harness) || (session && session.kind) || "";
    const dir = (d && d.dir) || (session && session.cwd) || "";
    const where = [
      target.mode === "dialog" ? el("span", { className: "act-machine", textContent: target.machine || shortHost(target.host || (d && d.host)), title: target.machine ? `On the machine ${target.machine}` : `Hub ${target.host || (d && d.host) || ""}` }) : null,
      workspace ? el("span", { className: "wk-ws", textContent: workspace.name }) : null,
      harness ? el("span", { className: `kind-tag ${harness}`, textContent: harness }) : null,
      d && d.model ? el("span", { className: "wk-model", textContent: d.model }) : null,
      dir ? el("span", { className: "wk-dir", textContent: dir, title: dir }) : null,
    ];
    $(target.mode === "drawer" ? "#wk-title" : "#wkd-title").textContent = title;
    $(target.mode === "drawer" ? "#wk-where" : "#wkd-where").replaceChildren(...where.filter(Boolean));
  }

  function render() {
    const target = view.target;
    if (!target) return;
    const body = bodyOf(target);
    renderHead();
    // The control the keys were on stays the one they are on.
    const held = body.contains(document.activeElement) ? document.activeElement.dataset.wk : null;
    const out = [];
    if (view.error) out.push(el("p", { className: "fw-problem wk-error", role: "alert", textContent: view.error }));
    const d = view.detail;
    if (d) {
      out.push(summary(d));
      const plan = (d.plans || []).find((p) => !p.owner);
      if (plan && plan.items.length) out.push(planSection(plan));
      out.push(tree(d));
    } else if (view.missing) {
      out.push(el("p", { className: "act-empty wk-empty", textContent: "Nothing recorded for this session yet." }));
    } else if (!view.error) {
      out.push(el("p", { className: "act-empty wk-empty", textContent: "Loading..." }));
    }
    body.replaceChildren(...out);
    if (held) {
      const again = [...body.querySelectorAll("[data-wk]")].find((node) => node.dataset.wk === held);
      if (again) again.focus({ preventScroll: true });
    }
  }

  // --- open / close --------------------------------------------------------

  function stop() {
    if (!view.target) return;
    view.seq++;
    if (view.abort) view.abort.abort();
    clearInterval(view.timer);
    clearTimeout(view.detailTimer);
    view.timer = 0;
    view.detailTimer = 0;
    view.target = null;
    view.detail = null;
    view.turns = new Map();
    view.events = [];
    view.after = false;
    view.again = false;
    view.older = false;
  }

  function start(target) {
    stop();
    view.seq++;
    view.abort = new AbortController();
    view.target = target;
    view.missing = false;
    view.error = "";
    view.loaded = false;
    view.complete = false;
    view.expanded = new Map();
    view.detailAt = Date.now();
    map.reset();
    $(target.mode === "drawer" ? "#wk-map" : "#wkd-map").replaceChildren(map.node);
    showMap();
    render();
    bodyOf(target).scrollTop = 0;
    loadDetail();
    loadNewest();
    view.timer = setInterval(() => {
      if (!view.target || document.visibilityState !== "visible") return;
      tick(view.target.mode === "drawer" ? drawer : dialog);
      if (mapShown) map.tick();
    }, 1000);
  }

  // A turn played back on the map: its events (older ones loaded first
  // when the turn began before those held).
  async function replayTurn(id) {
    const item = ((view.detail && view.detail.work) || []).find((w) => w.id === id);
    if (!item || !view.target) return;
    if (!view.complete && (!view.events.length || view.events[0].ts > item.started)) await loadOlder();
    const end = item.finished || Date.now();
    const events = view.events.filter((e) => e.ts >= item.started && e.ts <= end);
    // The context as the turn began: the last reading before it.
    const before = view.events.filter((e) => e.kind === "context.usage" && e.ts < item.started).pop();
    if (before) events.unshift({ ...before, ts: item.started });
    if (!mapShown) toggleMap();
    if (map.replay(events, promptTitle(item.title) || "turn")) {
      const host = $(view.target.mode === "drawer" ? "#wk-map" : "#wkd-map");
      host.scrollIntoView({ block: "nearest" });
    } else if (ctx.toast) ctx.toast("No events of that turn are held to replay.");
  }

  // The map shown or folded away (kept per browser).
  function showMap() {
    for (const [host, toggle] of [["#wk-map", "#wk-map-toggle"], ["#wkd-map", "#wkd-map-toggle"]]) {
      $(host).hidden = !mapShown;
      const button = $(toggle);
      button.setAttribute("aria-expanded", mapShown ? "true" : "false");
      button.title = mapShown ? "Fold the live map away" : "Show the live map: where the session's calls go";
      button.classList.toggle("on", mapShown);
    }
  }
  function toggleMap() {
    mapShown = !mapShown;
    ctx.store.set("work-map", mapShown);
    showMap();
    if (mapShown) map.tick();
  }

  // The drawer, for a session this browser knows (attach it first).
  function open(id) {
    const session = sessionById(id);
    if (!session) return;
    ctx.onOpen();
    if (dialog.open) dialog.close();
    start({ id, host: null, machine: session.machine || null, mode: "drawer" });
    drawer.hidden = false;
    renderPill();
    drawer.focus({ preventScroll: true });
  }

  // The read-only sheet, for a session held only as telemetry.
  function show(options) {
    closeDrawer();
    start({ id: options.session, host: options.host || null, machine: options.machine || null, label: options.label || "", mode: "dialog" });
    if (!dialog.open) dialog.showModal();
  }

  function closeDrawer() {
    if (!view.target || view.target.mode !== "drawer") return;
    stop();
    drawer.hidden = true;
    renderPill();
    ctx.focusTerminal();
  }

  function close() {
    closeDrawer();
    if (dialog.open) dialog.close();
  }

  const isOpen = () => !!view.target && view.target.mode === "drawer";

  function togglePill() {
    const id = state.attached;
    if (isOpen() && view.target.id === id) closeDrawer();
    else if (id && isAgent(sessionById(id))) open(id);
    else return false;
    return true;
  }

  function renderPill() {
    const pill = $("#work-toggle");
    const session = sessionById(state.attached);
    pill.hidden = !isAgent(session);
    if (pill.hidden) return;
    pill.setAttribute("aria-pressed", isOpen() && view.target.id === session.id ? "true" : "false");
  }

  // Called on every render: the pill; a drawer whose session is gone closes,
  // one whose session was renamed says so.
  function renderHeader() {
    renderPill();
    if (!isOpen()) return;
    if (!sessionById(view.target.id)) closeDrawer();
    else renderHead();
  }

  function paletteItems(cmd) {
    const session = sessionById(state.attached);
    if (isAgent(session)) cmd(`Work: ${session.name}`, "its plan, turns, subagents, and tool calls", () => open(session.id), "work");
  }

  // --- wiring --------------------------------------------------------------

  $("#wk-close").addEventListener("click", () => closeDrawer());
  $("#wk-map-toggle").addEventListener("click", () => toggleMap());
  $("#wkd-map-toggle").addEventListener("click", () => toggleMap());
  $("#wkd-close").addEventListener("click", () => close());
  drawer.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !document.querySelector("dialog[open]")) {
      event.stopPropagation();
      closeDrawer();
    }
  });
  dialog.addEventListener("close", () => {
    if (!dialog.open && view.target && view.target.mode === "dialog") stop();
  });

  return { open, show, close, closeDrawer, toggle: togglePill, togglePill, isOpen, onSnapshot, renderHeader, paletteItems, parts };
};

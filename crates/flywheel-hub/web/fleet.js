// flywheel hub: the Fleet board. Every agent session the hub has
// telemetry for (its own, and the ones other hubs forward to it when it
// is their collector), most urgent first: blocked, asking, failed, working, done,
// idle, ended. One row a session: its status, who it is, what it is
// doing, its declared plan's progress (observed counts when it declared
// none: never an invented percentage), context, cost and rate limits,
// errors, and when it last said anything.
//
// GET /api/telemetry/fleet pages this hub's board; each registered
// machine (machines.js) is asked for its own sessions through the proxy,
// the way search.js asks them, and a machine that does not answer is
// named. While the sheet is open, `telemetry` pushes on the events
// sockets update a row in place and the counts with it. A row opens the
// session and its Work drawer (work.js), or the read-only Work sheet for
// a session held only as telemetry.
//
// Runs the hub did not start (`fw hub capture install`: `claude -p`, SDK
// runs, a plain terminal's session) are counted per batch below the
// sessions: one row a batch (its launcher's, the hub session they ran
// under, or outside the hub) with its runs' statuses and what they spent
// against its budget; a batch row opens to list its runs.
//
// Lanes (the default view) place every session by what it needs from the
// operator now: needs you, moving, waiting on time (rate limited, or a
// long tool call), finished (nobody has looked since), idle; each lane
// grouped by project, batches of captured runs as one card. They come
// from GET /api/telemetry/lanes (the CLI's `fw hub fleet --lanes`), read
// again on each push (at most every 1.5 s) and every 15 s.
//
// Insights say where time and money went (GET /api/telemetry/insights,
// `fw hub insights`): per project, tool time by command, polling, what
// keeps failing, files written again and again, and the context tax.
// app-base.js creates it with its helpers.
"use strict";

window.FwHubFleet = function (ctx) {
  const { $, el, rawApi, machines, sessionById, workspaceById } = ctx;
  const { dollars, plural } = window.FwHubFmt;
  const parts = ctx.work.parts;

  const PAGE = 200;
  const PAGE_MAX = 1000;
  const HOURS = 24;
  const ORDER = ["blocked", "asking", "failed", "working", "done", "idle", "ended"];
  const rank = (status) => {
    const at = ORDER.indexOf(status);
    return at < 0 ? ORDER.length : at;
  };

  const ui = {
    filter: new Set(), // statuses shown (none: all)
    sources: [], // this hub (machine: null), then each machine
    rows: [], // { key, machine, snap }, most urgent first
    batches: [], // { key, machine, batch }, the latest active first
    opened: new Map(), // batch row key -> its runs' rows (null while loading)
    view: ["list", "insights"].includes(localStorage.getItem("fw-fleet-view")) ? localStorage.getItem("fw-fleet-view") : "lanes",
    insights: null, // the last insights page
    insightHours: 168,
    lanes: null, // the last lanes page
    lanesSeq: 0,
    lanesTimer: 0,
    counts: new Map(), // status -> { sessions, cost }
    copied: new Set(), // machines' hub ids this hub also holds rows of
    loading: false,
    error: "",
    seq: 0,
    timer: 0,
    reload: 0,
  };

  const dialog = $("#dlg-fleet");
  const list = $("#fl-list");
  const board = $("#fl-lanes");
  const report = $("#fl-insights");

  const keyOf = (machine, snap) => `${machine || ""}|${snap.host}|${snap.session}`;
  const costOf = (snap) => (snap.usage && snap.usage.cost_usd) || 0;
  const local = () => ui.sources.find((s) => !s.machine);
  const ownHost = () => (local() && local().host) || "";

  // --- fetching ------------------------------------------------------------

  function query(machine, offset, limit, batch) {
    const p = new URLSearchParams({ hours: String(HOURS), limit: String(limit), offset: String(offset) });
    if (ui.filter.size) p.set("status", [...ui.filter].join(","));
    if (batch) p.set("batch", batch);
    else p.set("rollup", "1");
    // A machine answers for the sessions that live on it; what it collects
    // from other hubs is those hubs' to show.
    if (machine) p.set("host", "local");
    return `${machines.prefix(machine)}/api/telemetry/fleet?${p}`;
  }

  async function ask(machine, offset, limit) {
    if (machine && !machines.reachable(machine)) {
      return { machine, ok: false, error: machines.linkOf(machine) || "offline", sessions: [], total: 0, counts: [], fetched: 0 };
    }
    try {
      const page = await rawApi("GET", query(machine, offset, limit));
      return { machine, ok: true, host: page.host, now: page.now, sessions: page.sessions || [], total: page.total || 0, counts: page.counts || [], hosts: page.hosts || 0, batches: page.batches || [], fetched: (page.sessions || []).length };
    } catch (error) {
      if (!machine) throw error;
      return { machine, ok: false, error: error.message, sessions: [], total: 0, counts: [], fetched: 0 };
    }
  }

  // A machine's own sessions are shown from the machine (they can be
  // attached there): this hub's copy of one, forwarded to it, is dropped.
  const machineHosts = () => new Set(ui.sources.filter((s) => s.machine && s.ok && s.host).map((s) => s.host));

  function bump(status, sessions, cost) {
    const at = ui.counts.get(status) || { sessions: 0, cost: 0 };
    at.sessions = Math.max(0, at.sessions + sessions);
    at.cost = Math.max(0, at.cost + cost);
    ui.counts.set(status, at);
  }

  const sortRows = () => ui.rows.sort((a, b) => rank(a.snap.status) - rank(b.snap.status) || b.snap.last_ts - a.snap.last_ts || (a.key < b.key ? -1 : 1));

  // Rows of `pages` (one per source) into the board; `more`: appended.
  function merge(pages, more) {
    if (!more) {
      ui.sources = pages.map((page) => ({ ...page, loaded: page.fetched }));
      ui.rows = [];
      ui.batches = [];
      ui.copied = new Set();
      ui.counts = new Map();
      for (const page of pages) {
        for (const count of page.counts) bump(count.status, count.sessions, count.cost_usd || 0);
      }
      const theirs = machineHosts();
      for (const page of pages) {
        for (const batch of page.batches || []) {
          if (!page.machine && theirs.has(batch.host)) continue;
          ui.batches.push({ key: `${page.machine || ""}|${batch.host}|${batch.key}`, machine: page.machine, batch });
        }
      }
      ui.batches.sort((a, b) => b.batch.last_ts - a.batch.last_ts);
      // An open batch's runs are read again with the board.
      for (const key of ui.opened.keys()) {
        const row = ui.batches.find((b) => b.key === key);
        if (row) openBatch(row, true);
        else ui.opened.delete(key);
      }
    } else {
      for (const page of pages) {
        const source = ui.sources.find((s) => s.machine === page.machine);
        if (!source) continue;
        source.loaded += page.fetched;
        source.total = page.total;
        if (!page.ok) {
          source.ok = false;
          source.error = page.error;
        }
      }
    }
    const theirs = machineHosts();
    const have = new Set(ui.rows.map((r) => r.key));
    for (const page of pages) {
      for (const snap of page.sessions) {
        const key = keyOf(page.machine, snap);
        if (have.has(key)) continue;
        if (!page.machine && theirs.has(snap.host)) {
          bump(snap.status, -1, -costOf(snap));
          ui.copied.add(snap.host);
          continue;
        }
        have.add(key);
        ui.rows.push({ key, machine: page.machine, snap });
      }
    }
    sortRows();
    const mine = pages.find((page) => !page.machine);
    if (mine) parts.setClock(mine.now);
  }

  async function load(more) {
    const seq = ++ui.seq;
    ui.loading = true;
    ui.error = "";
    renderStatus();
    try {
      let pages;
      if (more) {
        pages = await Promise.all(ui.sources.filter((s) => s.ok && s.total > s.loaded).map((s) => ask(s.machine, s.loaded, PAGE)));
      } else {
        // A reload keeps as many rows as were shown.
        const limit = (machine) => {
          const source = ui.sources.find((s) => s.machine === machine);
          return Math.min(PAGE_MAX, Math.max(PAGE, source ? source.loaded : 0));
        };
        pages = await Promise.all([null, ...machines.names()].map((machine) => ask(machine, 0, limit(machine))));
      }
      if (seq !== ui.seq) return;
      merge(pages, more);
    } catch (error) {
      if (seq !== ui.seq) return;
      ui.error = error.message;
    } finally {
      if (seq === ui.seq) {
        ui.loading = false;
        render();
      }
    }
  }

  const hasMore = () => ui.sources.some((s) => s.ok && s.total > s.loaded);

  // A change the rows held cannot place (a session outside the filter or
  // past the loaded page): the board is read again, at most every 1.5 s.
  function reloadSoon() {
    if (ui.reload) return;
    ui.reload = setTimeout(() => {
      ui.reload = 0;
      if (dialog.open) load(false);
    }, 1500);
  }

  // --- a row ---------------------------------------------------------------

  // The session of a row, when this browser can attach it: this hub's own,
  // or the machine's.
  function knownSession(row) {
    const session = sessionById(row.snap.session);
    if (!session) return null;
    if (row.machine) return session.machine === row.machine ? session : null;
    const own = ownHost() || (ui.lanes && ui.lanes.host) || "";
    return !session.machine && row.snap.host === own ? session : null;
  }

  const cell = (name, ...lines) => el("span", { className: `fl-c fl-c-${name}` }, ...lines);
  const dash = () => el("span", { className: "fl-none", textContent: "-" });

  function who(row) {
    const snap = row.snap;
    const session = knownSession(row);
    const workspace = session && workspaceById(session.workspace_id);
    const host = row.machine || (snap.host !== ownHost() ? parts.shortHost(snap.host) : "");
    return cell(
      "who",
      el(
        "span",
        { className: "fl-line" },
        el("b", { className: "fl-name", textContent: session ? session.name : snap.label || snap.session.slice(0, 8) }),
        host ? el("span", { className: "act-machine fl-host", textContent: host, title: row.machine ? `On the machine ${row.machine}` : `Hub ${snap.host}` }) : null,
      ),
      el(
        "span",
        { className: "fl-line fl-sub" },
        snap.harness ? el("span", { className: `kind-tag ${snap.harness}`, textContent: snap.harness }) : null,
        workspace ? el("span", { className: "fl-ws", textContent: workspace.name }) : null,
        snap.model ? el("span", { className: "fl-model", textContent: snap.model, title: snap.model }) : null,
      ),
      links(snap),
    );
  }

  // How it is tied to other sessions: what it continues, who opened it,
  // where its work went on.
  function links(snap) {
    const name = (id) => {
      const known = sessionById(id);
      return known ? known.name : id.slice(0, 8);
    };
    const bits = [];
    const l = snap.links || {};
    if (l.continues) bits.push(`continues ${name(l.continues)}`);
    if (l.captured) bits.push(l.batch ? `captured run · ${l.batch}` : "captured run");
    if (l.opened_by) bits.push(`${l.captured ? "ran under" : "opened by"} ${name(l.opened_by)}`);
    if (snap.continued_by) bits.push(`continued in ${name(snap.continued_by)}`);
    return bits.length ? el("span", { className: "fl-line fl-sub fl-links", textContent: bits.join(" · ") }) : null;
  }

  function doing(snap) {
    const tool = snap.tool;
    const text = tool ? `${tool.name}${tool.summary ? `: ${tool.summary}` : ""}` : "";
    return cell(
      "doing",
      snap.turn_title
        ? el("span", { className: "fl-line fl-title", textContent: snap.turn_title, title: snap.turn_title })
        : el("span", { className: "fl-line fl-none", textContent: snap.turns ? "-" : "no prompt yet" }),
      snap.ask
        ? el("span", { className: "fl-line fl-ask", textContent: snap.ask.text, title: snap.ask.command ? `${snap.ask.text}\n$ ${snap.ask.command}` : snap.ask.text })
        : tool ? el("span", { className: "fl-line fl-tool", textContent: text, title: text }) : null,
    );
  }

  function progress(snap) {
    const plan = snap.plan;
    if (plan && plan.total > 0) {
      return cell(
        "prog",
        el(
          "span",
          { className: "prog fl-prog", title: `The agent's ${plan.source} plan: ${plan.done} of ${plan.total} done` },
          parts.bar((plan.done / plan.total) * 100),
          el("small", { textContent: `${plan.done}/${plan.total}` }),
        ),
        el(
          "span",
          { className: "fl-line fl-sub" },
          plan.current ? el("span", { className: "fl-current", textContent: plan.current, title: plan.current }) : null,
          plan.revision > 1 ? el("span", { className: "wk-rev", textContent: `rev ${plan.revision}`, title: "Times the agent rewrote its plan" }) : null,
        ),
      );
    }
    // No declared plan: what was observed, nothing derived from it.
    return cell(
      "prog",
      el("span", { className: "fl-line fl-facts", textContent: plural(snap.turn_tools || 0, "call"), title: "Tool calls this turn (the agent declared no plan)" }),
      snap.subagents_total > 0
        ? el("span", { className: "fl-line fl-sub", textContent: `${snap.subagents_active || 0}/${snap.subagents_total} subagents`, title: "Subagents running / started" })
        : null,
    );
  }

  function cost(snap) {
    const rates = snap.rate || [];
    const top = rates.reduce((max, rate) => (!max || rate.used_pct > max.used_pct ? rate : max), null);
    const resets = (rate) => {
      const when = parts.resetsIn(rate);
      return `${parts.rateText(rate)}${when ? `, ${when}` : ""}`;
    };
    return cell(
      "cost",
      snap.usage
        ? el("span", { className: "fl-line fl-num", textContent: snap.usage.cost_usd == null ? "-" : dollars(snap.usage.cost_usd), title: parts.usageTitle(snap.usage) })
        : dash(),
      top
        ? el("span", { className: `fl-line fl-sub fl-num${top.used_pct >= 85 ? " hot" : ""}`, textContent: parts.rateText(top), title: `Rate limits: ${rates.map(resets).join(" · ")}` })
        : null,
    );
  }

  function last(snap) {
    return cell(
      "last",
      el("span", { className: "fl-line fl-num", title: `Last event ${new Date(snap.last_ts).toLocaleString()}` }, parts.live(snap.last_ts, 0, true)),
      snap.errors > 0
        ? el("span", { className: "fl-line fl-sub fl-err", textContent: plural(snap.errors, "error"), title: snap.last_error || "" })
        : null,
    );
  }

  function renderRow(row) {
    const snap = row.snap;
    return el(
      "li",
      { className: `fl-item${row.batch ? " fl-run" : ""}`, dataset: { key: row.key } },
      el(
        "button",
        { type: "button", className: `fl-row ${snap.status}`, onclick: () => follow(row) },
        cell("status", parts.statusNode(snap)),
        who(row),
        doing(snap),
        progress(snap),
        cell("ctx", snap.context ? parts.contextNode(snap.context) : dash()),
        cost(snap),
        last(snap),
      ),
    );
  }

  // --- a batch of captured runs ------------------------------------------

  function batchName(row) {
    const batch = row.batch;
    if (batch.label) return batch.label;
    if (batch.parent) {
      const known = sessionById(batch.parent);
      return `under ${batch.parent_name || (known ? known.name : batch.parent.slice(0, 8))}`;
    }
    return "outside the hub";
  }

  function renderBatch(row) {
    const batch = row.batch;
    const open = ui.opened.has(row.key);
    const counts = (batch.counts || []).map((c) => `${c.sessions} ${parts.lookOf(c.status).label}`).join(" · ");
    const top = (batch.counts || [])[0];
    const budget = batch.budget_usd;
    const spentPct = budget ? Math.min(100, (batch.cost_usd / budget) * 100) : 0;
    const host = row.machine || (batch.host !== ownHost() ? parts.shortHost(batch.host) : "");
    return el(
      "li",
      { className: `fl-item fl-batch${open ? " open" : ""}`, dataset: { key: row.key } },
      el(
        "button",
        {
          type: "button",
          className: `fl-row ${top ? top.status : "idle"}`,
          "aria-expanded": open ? "true" : "false",
          title: open ? "Hide its runs" : "List its runs",
          onclick: () => toggleBatch(row),
        },
        cell("status", top ? parts.statusNode({ status: top.status, since: batch.last_ts }) : dash()),
        cell(
          "who",
          el(
            "span",
            { className: "fl-line" },
            el("span", { className: "fl-caret", textContent: open ? "\u25be" : "\u25b8", "aria-hidden": "true" }),
            el("b", { className: "fl-name", textContent: batchName(row), title: batch.key }),
            host ? el("span", { className: "act-machine fl-host", textContent: host }) : null,
          ),
          el("span", { className: "fl-line fl-sub", textContent: `${plural(batch.sessions, "captured run")}` }),
        ),
        cell("doing", el("span", { className: "fl-line fl-facts", textContent: counts, title: counts })),
        cell(
          "prog",
          budget
            ? el(
                "span",
                { className: "prog fl-prog", title: `${dollars(batch.cost_usd)} of a ${dollars(budget)} budget` },
                parts.bar(spentPct, batch.over_budget),
                el("small", { className: batch.over_budget ? "fl-err" : "", textContent: `${Math.round((batch.cost_usd / budget) * 100)}%` }),
              )
            : el("span", { className: "fl-line fl-none", textContent: "no budget", title: "Its launcher set no FLYWHEEL_BATCH_BUDGET" }),
        ),
        cell("ctx", dash()),
        cell(
          "cost",
          el("span", { className: "fl-line fl-num", textContent: dollars(batch.cost_usd) }),
          budget
            ? el("span", { className: `fl-line fl-sub fl-num${batch.over_budget ? " hot" : ""}`, textContent: `of ${dollars(budget)}`, title: batch.over_budget ? `Over its ${dollars(budget)} budget` : `Its budget: ${dollars(budget)}` })
            : null,
        ),
        cell("last", el("span", { className: "fl-line fl-num", title: `Last event ${new Date(batch.last_ts).toLocaleString()}` }, parts.live(batch.last_ts, 0, true))),
      ),
    );
  }

  function batchRows(row) {
    const runs = ui.opened.get(row.key);
    if (runs === null) return [el("li", { className: "act-empty fl-run fl-tail-run", textContent: "Loading its runs..." })];
    if (!runs || !runs.length) return runs ? [el("li", { className: "act-empty fl-run", textContent: "No run in it with that status." })] : [];
    return runs.map(renderRow);
  }

  // Read one batch's runs (`again`: the board reloaded, keep what is shown
  // until they arrive).
  async function openBatch(row, again) {
    if (!again) ui.opened.set(row.key, null);
    const seq = ui.seq;
    try {
      const page = await rawApi("GET", query(row.machine, 0, PAGE_MAX, row.batch.key));
      if (seq !== ui.seq || !ui.opened.has(row.key)) return;
      ui.opened.set(row.key, (page.sessions || []).map((snap) => ({ key: keyOf(row.machine, snap), machine: row.machine, snap, batch: row.batch.key })));
    } catch (error) {
      if (!ui.opened.has(row.key)) return;
      ui.opened.set(row.key, []);
      ui.error = error.message;
    }
    render();
  }

  function toggleBatch(row) {
    if (ui.opened.has(row.key)) {
      ui.opened.delete(row.key);
      render();
      return;
    }
    openBatch(row, false);
    render();
  }

  // A session this browser can attach opens, with its Work drawer; one held
  // only as telemetry opens the read-only Work sheet over the board.
  async function follow(row) {
    const session = knownSession(row);
    if (session && session.live) {
      close();
      await ctx.openSession(session.id);
      ctx.openWork(session.id);
      return;
    }
    ctx.showWork({
      host: row.snap.host,
      session: row.snap.session,
      machine: row.machine,
      label: session ? session.name : row.snap.label || "",
    });
  }

  // --- rendering -----------------------------------------------------------

  function renderFilters() {
    const shown = ORDER.filter((status) => (ui.counts.get(status) || {}).sessions > 0 || ui.filter.has(status));
    for (const status of ui.counts.keys()) {
      if (!ORDER.includes(status) && ui.counts.get(status).sessions > 0) shown.push(status);
    }
    const held = document.activeElement && document.activeElement.closest("#fl-filters") ? document.activeElement.dataset.status : null;
    $("#fl-filters").replaceChildren(...shown.map((status) => {
      const look = parts.lookOf(status);
      return el(
        "button",
        {
          type: "button",
          className: `chip fl-chip ${status}`,
          "aria-pressed": ui.filter.has(status) ? "true" : "false",
          dataset: { status },
          title: ui.filter.has(status) ? `Showing ${look.label}: click to stop filtering by it` : `Show only ${look.label}`,
          onclick: () => {
            if (ui.filter.has(status)) ui.filter.delete(status);
            else ui.filter.add(status);
            renderFilters();
            load(false);
          },
        },
        el("span", { className: `dot ${look.dot}` }),
        el("span", { textContent: look.label }),
        el("b", { textContent: String((ui.counts.get(status) || { sessions: 0 }).sessions) }),
      );
    }));
    if (held) {
      const again = $(`#fl-filters [data-status="${held}"]`);
      if (again) again.focus({ preventScroll: true });
    }
  }

  function renderSummary() {
    const line = $("#fl-sum");
    let sessions = 0;
    let spent = 0;
    for (const count of ui.counts.values()) {
      sessions += count.sessions;
      spent += count.cost;
    }
    if (!local()) {
      line.replaceChildren();
      return;
    }
    // Hubs heard from: the ones this hub holds rows of, and each machine
    // it holds none of.
    // With every row here, the rows say it (a pushed row may be a new hub's).
    const hosts = !ui.filter.size && !hasMore()
      ? new Set(ui.rows.map((r) => r.snap.host)).size
      : (local().hosts || 0) + ui.sources.filter((s) => s.machine && s.ok && s.total > 0 && !ui.copied.has(s.host)).length;
    const bits = [plural(sessions, "session"), dollars(spent)];
    if (hosts > 1) bits.push(`${hosts} hosts`);
    if (ui.filter.size) bits.push(`showing ${ui.rows.length}`);
    line.replaceChildren(el("b", { textContent: `last ${HOURS} h` }), ` ${bits.join(" · ")}`);
  }

  function renderStatus() {
    const bits = [];
    if (ui.loading) bits.push("loading...");
    if (ui.error) bits.push(ui.error);
    const missed = ui.sources.filter((s) => s.machine && !s.ok);
    if (missed.length) bits.push(`partial: ${missed.map((s) => `${s.machine} (${s.error})`).join(", ")}`);
    const line = $("#fl-status");
    line.textContent = bits.join(" · ");
    line.classList.toggle("bad", !!ui.error || missed.length > 0);
  }

  const HEAD = [["status", "status"], ["who", "session"], ["doing", "doing"], ["prog", "progress"], ["ctx", "context"], ["cost", "cost"], ["last", "last"]];

  function tail() {
    const out = [];
    if (!ui.rows.length && !ui.batches.length && !ui.loading && !ui.error) {
      out.push(el("li", { className: "act-empty fl-tail", textContent: ui.filter.size ? "No session with that status." : "No agent session has reported yet." }));
    }
    if (hasMore()) {
      out.push(el("li", { className: "act-more fl-tail" }, el("button", {
        type: "button",
        className: "slab ghost",
        textContent: ui.loading ? "Loading..." : "Show more",
        disabled: ui.loading,
        onclick: () => load(true),
      })));
    }
    return out;
  }

  function render() {
    renderViews();
    const lanes = ui.view === "lanes";
    const insights = ui.view === "insights";
    $("#fl-filters").hidden = lanes || insights;
    list.hidden = lanes || insights;
    board.hidden = !lanes;
    report.hidden = !insights;
    if (insights) {
      renderStatus();
      const hours = ui.insightHours;
      $("#fl-sum").replaceChildren(el("b", { textContent: hours % 24 ? `last ${hours} h` : `last ${hours / 24 === 1 ? "24 h" : `${hours / 24} days`}` }), " where time and money went");
      renderInsights();
      return;
    }
    if (lanes) {
      renderStatus();
      const page = ui.lanes;
      const bits = page ? [plural(page.total, "session")] : [];
      if (page && page.runs) bits.push(plural(page.runs, "captured run"));
      if (page && page.total > page.placed) bits.push(`lanes place the ${page.placed} most urgent`);
      $("#fl-sum").replaceChildren(el("b", { textContent: `last ${HOURS} h` }), bits.length ? ` ${bits.join(" · ")}` : "");
      renderLanes();
      return;
    }
    renderFilters();
    renderSummary();
    renderStatus();
    const held = heldKey();
    const head = ui.rows.length || ui.batches.length
      ? [el("li", { className: "fl-head", "aria-hidden": "true" }, HEAD.map(([name, label]) => el("span", { className: `fl-c fl-c-${name}`, textContent: label })))]
      : [];
    const batches = ui.batches.length
      ? [
          el("li", { className: "fl-section", textContent: `Captured runs · ${plural(ui.batches.reduce((n, b) => n + b.batch.sessions, 0), "run")} in ${ui.batches.length} ${ui.batches.length === 1 ? "batch" : "batches"}` }),
          ...ui.batches.flatMap((row) => [renderBatch(row), ...batchRows(row)]),
        ]
      : [];
    list.replaceChildren(...head, ...ui.rows.map(renderRow), ...tail(), ...batches);
    refocus(held);
  }

  const itemOf = (key) => [...list.querySelectorAll(".fl-item")].find((node) => node.dataset.key === key);
  function heldKey() {
    const item = document.activeElement && document.activeElement.closest(".fl-item");
    return item && list.contains(item) ? item.dataset.key : null;
  }
  function refocus(key) {
    const item = key && itemOf(key);
    if (item) item.firstElementChild.focus({ preventScroll: true });
  }

  // One row changed: it is redrawn where it stands, or moved to where its
  // urgency now puts it; the rest of the list is left alone.
  function renderOne(row, removed) {
    renderFilters();
    renderSummary();
    const held = heldKey();
    const old = itemOf(row.key);
    if (removed) {
      if (old) old.remove();
    } else {
      const fresh = renderRow(row);
      if (old) old.remove();
      const next = ui.rows[ui.rows.indexOf(row) + 1];
      const before = (next && itemOf(next.key)) || list.querySelector(".fl-tail") || list.querySelector(".fl-section");
      list.insertBefore(fresh, before || null);
    }
    if (!list.querySelector(".fl-head") || !ui.rows.length) render();
    else refocus(held);
  }

  // --- lanes ---------------------------------------------------------------

  async function loadLanes() {
    const seq = ++ui.lanesSeq;
    try {
      const page = await rawApi("GET", `/api/telemetry/lanes?hours=${HOURS}`);
      if (seq !== ui.lanesSeq || !dialog.open) return;
      ui.lanes = page;
      ui.error = "";
      parts.setClock(page.now);
    } catch (error) {
      if (seq !== ui.lanesSeq) return;
      ui.error = error.message;
    }
    if (ui.view === "lanes") render();
  }

  function lanesSoon() {
    if (ui.lanesTimer) return;
    ui.lanesTimer = setTimeout(() => {
      ui.lanesTimer = 0;
      if (dialog.open && ui.view === "lanes") loadLanes();
    }, 1500);
  }

  // A batch card: its runs, in the list.
  function openBatchInList(batch) {
    ui.opened.set(`|${batch.host}|${batch.key}`, null);
    if (ui.view === "list") load(false);
    else setView("list");
  }

  function card(item) {
    const snap = item.session;
    const batch = item.batch;
    const status = snap ? snap.status : (batch.counts[0] || {}).status || "idle";
    const look = parts.lookOf(status);
    const name = snap
      ? (() => {
          const known = snap.host === ui.lanes.host ? sessionById(snap.session) : null;
          return known && !known.machine ? known.name : snap.label || snap.session.slice(0, 8);
        })()
      : batchName({ batch });
    const host = snap && snap.host !== ui.lanes.host ? parts.shortHost(snap.host) : "";
    return el(
      "button",
      {
        type: "button",
        className: `fl-card ${status}${batch ? " fl-card-batch" : ""}${batch && batch.over_budget ? " over" : ""}`,
        title: item.why,
        onclick: () => (snap ? follow({ machine: null, snap }) : openBatchInList(batch)),
      },
      el(
        "span",
        { className: "fl-card-top" },
        el("span", { className: `dot ${look.dot}` }),
        el("b", { className: "fl-name", textContent: name }),
        host ? el("span", { className: "act-machine fl-host", textContent: host }) : null,
        el("span", { className: "fl-card-age fl-num" }, parts.live(item.since, 0, true)),
      ),
      el("span", { className: "fl-card-why", textContent: item.why }),
      batch
        ? el("span", { className: "fl-card-sub fl-num", textContent: `${dollars(batch.cost_usd)}${batch.budget_usd ? ` of ${dollars(batch.budget_usd)}` : ""} · ${plural(batch.sessions, "run")}` })
        : snap && snap.usage && snap.usage.cost_usd != null
          ? el("span", { className: "fl-card-sub fl-num", textContent: `${dollars(snap.usage.cost_usd)}${snap.context ? ` · ctx ${Math.round(snap.context.pct)}%` : ""}` })
          : null,
    );
  }

  function renderLanes() {
    const page = ui.lanes;
    if (!page) {
      board.replaceChildren(el("p", { className: "act-empty", textContent: ui.error ? "" : "Loading..." }));
      return;
    }
    board.replaceChildren(...page.lanes.map((lane) => el(
      "section",
      { className: `fl-lane fl-lane-${lane.id}`, "aria-label": `${lane.title}: ${lane.count}` },
      el("h3", { className: "fl-lane-head" }, el("span", { textContent: lane.title }), el("b", { className: "fl-num", textContent: String(lane.count) })),
      ...(lane.groups.length
        ? lane.groups.map((group) => el(
            "div",
            { className: "fl-group" },
            el("h4", { className: "fl-group-head", textContent: group.project }),
            ...group.cards.map(card),
          ))
        : [el("p", { className: "fl-lane-empty", textContent: "Nothing" })]),
      lane.count > lane.groups.reduce((n, g) => n + g.cards.length, 0)
        ? el("p", { className: "fl-lane-empty", textContent: `and ${lane.count - lane.groups.reduce((n, g) => n + g.cards.length, 0)} more: the list has them all` })
        : null,
    )));
  }

  // --- insights ------------------------------------------------------------

  async function loadInsights() {
    const seq = ++ui.lanesSeq;
    try {
      const page = await rawApi("GET", `/api/telemetry/insights?hours=${ui.insightHours}`);
      if (seq !== ui.lanesSeq || !dialog.open) return;
      ui.insights = page.insights;
      ui.error = "";
    } catch (error) {
      if (seq !== ui.lanesSeq) return;
      ui.error = error.message;
    }
    if (ui.view === "insights") render();
  }

  const took = (ms) => {
    const s = Math.floor((ms || 0) / 1000);
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.floor(s / 60)}m`;
    return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
  };
  const tokens = (n) => (n >= 1e7 ? `${Math.round(n / 1e6)}M` : n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}k` : String(n || 0));
  const pctOf = (part, whole) => (whole > 0 ? Math.round((part / whole) * 100) : 0);
  const shapeName = (row) => row.shape || row.tool;

  // One section: a heading, a note, and rows of cells.
  function section(title, note, head, rows) {
    if (!rows.length) return null;
    return el(
      "section",
      { className: "fl-ins" },
      el("h3", { className: "fl-lane-head" }, el("span", { textContent: title })),
      note ? el("p", { className: "fl-ins-note", textContent: note }) : null,
      el(
        "table",
        { className: "fl-ins-table" },
        el("thead", {}, el("tr", {}, ...head.map(([label, num]) => el("th", { className: num ? "fl-num" : "", textContent: label })))),
        el("tbody", {}, ...rows.map((cells) => el("tr", {}, ...cells.map(([text, num, extra]) => el("td", { className: `${num ? "fl-num" : ""} ${extra || ""}`.trim(), textContent: text, title: text }))))),
      ),
    );
  }

  function renderInsights() {
    const found = ui.insights;
    const windows = el("div", { className: "search-filters fl-filters" }, ...[[24, "24 h"], [168, "7 days"], [720, "30 days"]].map(([hours, label]) => el("button", {
      type: "button",
      className: "chip fl-chip",
      "aria-pressed": String(ui.insightHours === hours),
      textContent: label,
      onclick: () => {
        ui.insightHours = hours;
        loadInsights();
        render();
      },
    })));
    if (!found) {
      report.replaceChildren(windows, el("p", { className: "act-empty", textContent: ui.error ? "" : "Loading..." }));
      return;
    }
    if (!found.projects.length) {
      report.replaceChildren(windows, el("p", { className: "act-empty", textContent: "Nothing recorded in this window." }));
      return;
    }
    const toolMs = found.projects.reduce((n, p) => n + p.tool_ms, 0);
    const poll = found.polling;
    const ctxTax = found.context;
    report.replaceChildren(
      windows,
      section("Projects", "", [["project"], ["sessions", 1], ["tool time", 1], ["calls", 1], ["failed", 1], ["cost", 1]],
        found.projects.map((p) => [[p.project], [String(p.sessions), 1], [took(p.tool_ms), 1], [String(p.calls), 1], [String(p.errors), 1, p.errors ? "fl-err" : ""], [dollars(p.cost_usd), 1]])),
      section("Time by command", poll.ms ? `Polling (sleep, until, while loops) took ${took(poll.ms)}: ${pctOf(poll.ms, poll.shell_ms)}% of shell time. Blocking on the event itself (gh run watch, wait) or a background run frees the turn.` : "",
        [["command"], ["time", 1], ["share", 1], ["calls", 1], ["sessions", 1], ["failed", 1]],
        found.time.map((row) => [[shapeName(row), 0, row.shape === "poll" ? "fl-err" : ""], [took(row.ms), 1], [`${pctOf(row.ms, toolMs)}%`, 1], [String(row.calls), 1], [String(row.sessions), 1], [String(row.errors || ""), 1]])),
      section("Keeps failing", "", [["command"], ["failed", 1], ["of", 1], ["last error"]],
        found.failing.map((row) => [[shapeName(row)], [String(row.errors), 1, "fl-err"], [String(row.calls), 1], [row.last_error || "", 0, "fl-ins-err"]])),
      section("Written again and again", "Files written whole three times or more: keep one script and run it instead.", [["file"], ["writes", 1], ["sessions", 1]],
        found.rewritten.map((row) => [[row.path], [String(row.writes), 1], [String(row.sessions), 1]])),
      section("Context tax", `${tokens(ctxTax.cache_read)} tokens re-read from cache for ${tokens(ctxTax.output)} written${ctxTax.priced_cost_usd ? ` (${pctOf(ctxTax.cache_read_usd, ctxTax.priced_cost_usd)}% of spend where priced)` : ""}. A handoff or /compact sooner reads less.`,
        [["session"], ["project"], ["re-read", 1], ["written", 1], ["re-read cost", 1], ["context now", 1]],
        ctxTax.sessions.map((row) => [[row.label || row.session.slice(0, 8)], [row.project], [tokens(row.cache_read), 1], [tokens(row.output), 1], [row.cache_read_usd == null ? "-" : dollars(row.cache_read_usd), 1], [row.context ? tokens(row.context) : "-", 1]])),
    );
  }

  function renderViews() {
    $("#fl-views").replaceChildren(...[["lanes", "Lanes"], ["list", "List"], ["insights", "Insights"]].map(([key, label]) => el(
      "button",
      {
        type: "button",
        role: "tab",
        className: `rv-tab${ui.view === key ? " on" : ""}`,
        "aria-selected": String(ui.view === key),
        onclick: () => setView(key),
      },
      label,
    )));
  }

  function setView(view) {
    if (ui.view === view) return;
    ui.view = view;
    localStorage.setItem("fw-fleet-view", view);
    if (view === "lanes") loadLanes();
    else if (view === "insights") loadInsights();
    else load(false);
    render();
  }

  // --- live ----------------------------------------------------------------

  // A `telemetry` push (this hub's, or `machine`'s) while the board is open.
  function onSnapshot(snap, machine) {
    if (!dialog.open || !snap || typeof snap !== "object" || !snap.session) return;
    if (ui.view === "lanes") {
      if (!machine) lanesSoon();
      return;
    }
    if (ui.view === "insights") return;
    const source = ui.sources.find((s) => s.machine === (machine || null));
    if (!source) return;
    if (!source.ok) {
      // The machine is back: what the board missed of it is fetched.
      reloadSoon();
      return;
    }
    if (machine ? snap.host !== source.host : machineHosts().has(snap.host)) return;
    if (snap.links && snap.links.captured) {
      // A captured run is counted in its batch: the rollups are read again.
      reloadSoon();
      return;
    }
    const key = keyOf(machine, snap);
    const at = ui.rows.findIndex((r) => r.key === key);
    if (at < 0) {
      // Unknown here: new, or one the filter or the page kept out.
      if (ui.filter.size || hasMore()) {
        reloadSoon();
        return;
      }
      const row = { key, machine: machine || null, snap };
      bump(snap.status, 1, costOf(snap));
      source.loaded += 1;
      source.total += 1;
      ui.rows.push(row);
      sortRows();
      renderOne(row, false);
      return;
    }
    const row = ui.rows[at];
    if (snap.seq < row.snap.seq) return;
    bump(row.snap.status, -1, -costOf(row.snap));
    bump(snap.status, 1, costOf(snap));
    row.snap = snap;
    if (ui.filter.size && !ui.filter.has(snap.status)) {
      ui.rows.splice(at, 1);
      source.loaded = Math.max(0, source.loaded - 1);
      source.total = Math.max(0, source.total - 1);
      renderOne(row, true);
      return;
    }
    sortRows();
    renderOne(row, false);
  }

  // A machine's events socket (re)opened: a board that missed it asks again.
  function onMachine(name) {
    if (dialog.open && ui.sources.some((s) => s.machine === name && !s.ok)) load(false);
  }

  // --- open / close --------------------------------------------------------

  function open() {
    if (ctx.onOpen) ctx.onOpen();
    document.body.classList.remove("side-open");
    ui.rows = [];
    ui.batches = [];
    ui.opened = new Map();
    ui.sources = [];
    ui.counts = new Map();
    ui.error = "";
    ui.loading = ui.view === "list";
    ui.lanes = null;
    render();
    if (!dialog.open) dialog.showModal();
    ui.insights = null;
    if (ui.view === "lanes") loadLanes();
    else if (ui.view === "insights") loadInsights();
    else load(false);
    clearInterval(ui.timer);
    let ticks = 0;
    ui.timer = setInterval(() => {
      if (document.visibilityState !== "visible") return;
      parts.tick(ui.view === "lanes" ? board : list);
      // A tool call becomes a wait with time alone.
      if (ui.view === "lanes" && ++ticks % 15 === 0) loadLanes();
    }, 1000);
  }

  function close() {
    if (dialog.open) dialog.close();
  }

  dialog.addEventListener("close", () => {
    // Closed and opened again at once: the open board keeps its timers.
    if (dialog.open) return;
    ui.seq++;
    ui.loading = false;
    clearInterval(ui.timer);
    clearTimeout(ui.reload);
    clearTimeout(ui.lanesTimer);
    ui.lanesSeq++;
    ui.timer = 0;
    ui.reload = 0;
    ui.lanesTimer = 0;
  });
  $("#fl-close").addEventListener("click", () => close());

  function paletteItems(cmd) {
    cmd("Fleet board", "every agent session in lanes: needs you, moving, waiting, finished, idle", () => open(), "fleet");
  }

  return { open, close, isOpen: () => dialog.open, onSnapshot, onMachine, paletteItems };
};

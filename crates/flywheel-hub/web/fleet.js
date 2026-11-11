// flywheel hub: the Fleet board. Every agent session the hub has
// telemetry for (its own, and the ones other hubs forward to it when it
// is their collector), most urgent first: blocked, failed, working, done,
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
// app-base.js creates it with its helpers.
"use strict";

window.FwHubFleet = function (ctx) {
  const { $, el, rawApi, machines, sessionById, workspaceById } = ctx;
  const { dollars, plural } = window.FwHubFmt;
  const parts = ctx.work.parts;

  const PAGE = 200;
  const PAGE_MAX = 1000;
  const HOURS = 24;
  const ORDER = ["blocked", "failed", "working", "done", "idle", "ended"];
  const rank = (status) => {
    const at = ORDER.indexOf(status);
    return at < 0 ? ORDER.length : at;
  };

  const ui = {
    filter: new Set(), // statuses shown (none: all)
    sources: [], // this hub (machine: null), then each machine
    rows: [], // { key, machine, snap }, most urgent first
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

  const keyOf = (machine, snap) => `${machine || ""}|${snap.host}|${snap.session}`;
  const costOf = (snap) => (snap.usage && snap.usage.cost_usd) || 0;
  const local = () => ui.sources.find((s) => !s.machine);
  const ownHost = () => (local() && local().host) || "";

  // --- fetching ------------------------------------------------------------

  function query(machine, offset, limit) {
    const p = new URLSearchParams({ hours: String(HOURS), limit: String(limit), offset: String(offset) });
    if (ui.filter.size) p.set("status", [...ui.filter].join(","));
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
      return { machine, ok: true, host: page.host, now: page.now, sessions: page.sessions || [], total: page.total || 0, counts: page.counts || [], hosts: page.hosts || 0, fetched: (page.sessions || []).length };
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
      ui.copied = new Set();
      ui.counts = new Map();
      for (const page of pages) {
        for (const count of page.counts) bump(count.status, count.sessions, count.cost_usd || 0);
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
    return !session.machine && row.snap.host === ownHost() ? session : null;
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
    );
  }

  function doing(snap) {
    const tool = snap.tool;
    const text = tool ? `${tool.name}${tool.summary ? `: ${tool.summary}` : ""}` : "";
    return cell(
      "doing",
      snap.turn_title
        ? el("span", { className: "fl-line fl-title", textContent: snap.turn_title, title: snap.turn_title })
        : el("span", { className: "fl-line fl-none", textContent: snap.turns ? "-" : "no prompt yet" }),
      tool ? el("span", { className: "fl-line fl-tool", textContent: text, title: text }) : null,
    );
  }

  function progress(snap) {
    const plan = snap.plan;
    if (plan && plan.total > 0) {
      return cell(
        "prog",
        el(
          "span",
          { className: "fwb-prog fl-prog", title: `The agent's ${plan.source} plan: ${plan.done} of ${plan.total} done` },
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
      { className: "fl-item", dataset: { key: row.key } },
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
    if (!ui.rows.length && !ui.loading && !ui.error) {
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
    renderFilters();
    renderSummary();
    renderStatus();
    const held = heldKey();
    const head = ui.rows.length
      ? [el("li", { className: "fl-head", "aria-hidden": "true" }, HEAD.map(([name, label]) => el("span", { className: `fl-c fl-c-${name}`, textContent: label })))]
      : [];
    list.replaceChildren(...head, ...ui.rows.map(renderRow), ...tail());
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
      const before = (next && itemOf(next.key)) || list.querySelector(".fl-tail");
      list.insertBefore(fresh, before || null);
    }
    if (!list.querySelector(".fl-head") || !ui.rows.length) render();
    else refocus(held);
  }

  // --- live ----------------------------------------------------------------

  // A `telemetry` push (this hub's, or `machine`'s) while the board is open.
  function onSnapshot(snap, machine) {
    if (!dialog.open || !snap || typeof snap !== "object" || !snap.session) return;
    const source = ui.sources.find((s) => s.machine === (machine || null));
    if (!source) return;
    if (!source.ok) {
      // The machine is back: what the board missed of it is fetched.
      reloadSoon();
      return;
    }
    if (machine ? snap.host !== source.host : machineHosts().has(snap.host)) return;
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
    ui.sources = [];
    ui.counts = new Map();
    ui.error = "";
    ui.loading = true;
    render();
    if (!dialog.open) dialog.showModal();
    load(false);
    clearInterval(ui.timer);
    ui.timer = setInterval(() => {
      if (document.visibilityState === "visible") parts.tick(list);
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
    ui.timer = 0;
    ui.reload = 0;
  });
  $("#fl-close").addEventListener("click", () => close());

  function paletteItems(cmd) {
    cmd("Fleet board", "every agent session, most urgent first: status, plan, context, cost", () => open(), "fleet");
  }

  return { open, close, isOpen: () => dialog.open, onSnapshot, onMachine, paletteItems };
};

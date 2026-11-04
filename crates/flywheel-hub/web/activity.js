// flywheel hub: the Activity view and "while you were away".
//
// Activity (palette, the sidebar's activity chip, the header's activity
// pill -- the phone's way in): this hub's journal merged with every
// machine's (GET /api/activity, cursor-paged, machine-labelled), grouped
// by day and workspace, filterable by workspace, session, and kind. Each
// row opens what it is about: a finished turn the transcript at its
// reply, suggested commands the Commands drawer at the block, a review
// action the Review drawer, a graded fan-out its board, a queue event the
// queue, a recording its replay; anything else attaches the session. New
// rows arrive as `activity` pushes on the events socket.
//
// While you were away: the events socket sends `away` {since, until} once
// when this browser (its per-device client id) comes back after a while;
// the digest of that span (GET /api/activity/digest, every machine
// merged) shows as a dismissible card: turns finished, agents blocked
// now, fan-outs graded (winner), commands suggested, cost.
// app-base.js creates it with its helpers (crates/flywheel-hub/src/activity.rs).
"use strict";

window.FwHubActivity = function (ctx) {
  const { $, el, icon, rawApi, state, sessionById, workspaceById, toast, store } = ctx;

  const PAGE = 60;
  const KINDS = [
    ["", "every kind"],
    ["turn.finished", "finished turns"],
    ["status.changed", "blocked"],
    ["commands.suggested", "suggested commands"],
    ["review", "reviews"],
    ["fanout", "fan-outs"],
    ["session", "sessions"],
    ["usage", "usage"],
    ["queue", "prompt queues"],
    ["handoff", "handoffs"],
    ["share,record", "shares and recordings"],
    ["plugin", "plugins"],
    ["hub", "hub"],
  ];
  // The tag each kind's rows carry (the timeline's left column).
  const TAG = {
    "turn.finished": "turn",
    "status.changed": "blocked",
    "commands.suggested": "cmds",
    usage: "spend",
    "fanout.finished": "fan-out",
    "handoff.sent": "handoff",
    "plugin.ran": "plugin",
  };

  const ui = {
    ws: "",
    session: "",
    kind: "",
    since: "",
    rows: [],
    next: null,
    loading: false,
    error: "",
    machines: [],
    seq: 0,
    sumSeq: 0,
    // Rows since the view was last open (the pill's count).
    unread: 0,
    // The newest of this hub's row ids the view has shown (kept per
    // browser: a reload still counts what came since).
    seenId: store.get("activity-seen", null),
  };

  const dialog = $("#dlg-activity");
  const list = $("#act-list");

  // --- fetching -----------------------------------------------------------

  function query(cursor) {
    const p = new URLSearchParams({ limit: String(PAGE) });
    if (ui.ws) p.set("ws", ui.ws);
    if (ui.session) p.set("session", ui.session);
    if (ui.kind) p.set("kind", ui.kind);
    if (ui.since) p.set("since", ui.since);
    if (cursor) p.set("cursor", cursor);
    return `/api/activity?${p}`;
  }

  async function load(more) {
    const seq = ++ui.seq;
    ui.loading = true;
    ui.error = "";
    renderStatus();
    try {
      const page = await rawApi("GET", query(more ? ui.next : null));
      if (seq !== ui.seq) return;
      ui.rows = more ? ui.rows.concat(page.rows) : page.rows;
      ui.next = page.next;
      ui.machines = page.machines || [];
      markSeen(ui.rows);
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

  // The view's own digest: what the shown span comes to.
  async function loadSummary() {
    const seq = ++ui.sumSeq;
    const p = new URLSearchParams({ since: ui.since || "24h" });
    if (ui.ws) p.set("ws", ui.ws);
    if (ui.session) p.set("session", ui.session);
    try {
      const digest = await rawApi("GET", `/api/activity/digest?${p}`);
      if (seq === ui.sumSeq) renderSummary(digest);
    } catch (_) {
      if (seq === ui.sumSeq) $("#act-sum").replaceChildren();
    }
  }

  function renderSummary(digest) {
    const n = (v) => Number(v || 0);
    const { dollars } = window.FwHubFmt;
    const plural = (count, one, many) => `${count} ${count === 1 ? one : many}`;
    const bits = [
      plural(n(digest.turns_finished), "turn", "turns"),
      `${(digest.blocked_now || []).length} blocked now`,
      plural((digest.fanouts_graded || []).length, "fan-out graded", "fan-outs graded"),
      plural(n(digest.commands_suggested), "command", "commands"),
      dollars(n(digest.cost_usd)),
    ];
    $("#act-sum").replaceChildren(
      el("b", { textContent: ui.since ? "since then" : "last 24 h" }),
      ` ${bits.join(" · ")}`,
    );
  }

  // This hub's newest row shown: counted as seen.
  function markSeen(rows) {
    const newest = rows.reduce((max, r) => (!r.machine && r.id > max ? r.id : max), ui.seenId || 0);
    if (newest !== ui.seenId) {
      ui.seenId = newest;
      store.set("activity-seen", newest);
    }
  }

  // On load: the rows since this browser last looked (the pill's count).
  async function countUnread() {
    try {
      const page = await rawApi("GET", "/api/activity?machines=none&limit=100");
      if (dialog.open) return;
      if (ui.seenId === null) {
        markSeen(page.rows);
        return;
      }
      ui.unread = page.rows.filter((r) => r.id > ui.seenId && r.kind !== "usage").length;
      renderPill();
    } catch (_) {
      // The count is a hint; the view still opens.
    }
  }

  // --- what a row opens ---------------------------------------------------

  const sessionOf = (row) => (row.session_id ? sessionById(row.session_id) : null);

  // {label, run} for the row's object, or null when it has none here.
  function target(row) {
    const data = row.data || {};
    const session = sessionOf(row);
    const attach = session && session.live ? { label: "attach", run: () => ctx.openSession(session.id) } : null;
    const local = !row.machine;
    if (row.kind === "fanout.finished" && data.fanout_id && local) {
      return { label: "board", run: () => ctx.openFanout(data.fanout_id) };
    }
    if (!session) return null;
    if (row.kind === "turn.finished" && Number.isInteger(data.message)) {
      return {
        label: "transcript",
        back: true,
        run: () => ctx.openTranscript(
          {
            session_id: session.id,
            session: session.name,
            workspace: row.workspace || "",
            kind: session.kind,
            state: session.state,
            machine: session.machine || null,
            conversation: session.agent_session_id || null,
          },
          data.message,
        ),
      };
    }
    if (row.kind === "commands.suggested" && Number.isInteger(data.seq)) {
      return { label: "commands", run: async () => {
        await ctx.openSession(session.id);
        ctx.openCommandsAt(session.id, data.seq, !data.shell);
      } };
    }
    if (row.kind.startsWith("review.") && row.kind !== "review.requested") {
      return { label: "review", run: async () => {
        await ctx.openSession(session.id);
        ctx.openReview(session.id);
      } };
    }
    if (row.kind.startsWith("queue.")) {
      return { label: "queue", run: async () => {
        await ctx.openSession(session.id);
        ctx.openQueue(session.id);
      } };
    }
    if (row.kind === "record.stopped" && data.recording && local) {
      return { label: "replay", run: () => window.open(`/replay#${encodeURIComponent(data.recording)}`, "_blank", "noopener") };
    }
    return attach;
  }

  // --- rendering ----------------------------------------------------------

  const clock = (iso) => new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

  function dayLabel(iso) {
    const day = new Date(iso);
    const today = new Date();
    const yesterday = new Date(Date.now() - 86400000);
    const same = (a, b) => a.toDateString() === b.toDateString();
    if (same(day, today)) return "Today";
    if (same(day, yesterday)) return "Yesterday";
    return day.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" });
  }

  function tagOf(row) {
    return TAG[row.kind] || row.kind.split(".")[0];
  }

  function renderRow(row) {
    const go = target(row);
    const session = sessionOf(row);
    const who = row.session || "";
    const node = el(
      "li",
      { className: `act-row k-${row.kind.replace(/\./g, "-")}${go ? " linked" : ""}` },
      el("time", { className: "act-time", dateTime: row.at, textContent: clock(row.at), title: new Date(row.at).toLocaleString() }),
      el("span", { className: "act-tag", textContent: tagOf(row) }),
      el(
        "span",
        { className: "act-what" },
        who ? el("b", { className: "act-who", textContent: who }) : null,
        session && session.kind !== "shell" ? icon(session.kind) : null,
        el("span", { className: "act-text", textContent: row.text }),
      ),
      go
        ? el("button", {
          type: "button",
          className: "act-go",
          textContent: go.label,
          title: `Open: ${go.label}`,
          onclick: (event) => {
            event.stopPropagation();
            follow(go);
          },
        })
        : el("span", { className: "act-go none", textContent: session ? "" : row.session_id ? "gone" : "" }),
    );
    if (go) {
      node.tabIndex = 0;
      node.addEventListener("click", () => follow(go));
      node.addEventListener("keydown", (event) => {
        if (event.key === "Enter") follow(go);
      });
    }
    return node;
  }

  // Open what a row is about; a transcript closed (Esc, its x) comes back
  // here, filters and place kept.
  function follow(go) {
    close();
    go.run();
    if (!go.back) return;
    const shown = $("#dlg-transcript");
    let cancelled = false;
    const stop = new AbortController();
    shown.returnValue = "";
    shown.addEventListener("cancel", () => { cancelled = true; }, { signal: stop.signal });
    shown.addEventListener("close", () => {
      stop.abort();
      if ((cancelled || shown.returnValue === "cancel") && !dialog.open) dialog.showModal();
    }, { signal: stop.signal });
  }

  // Rows by day, then by hour; in an hour, each workspace (and machine)
  // under one head, newest first.
  function render() {
    renderFilters();
    renderStatus();
    const out = [];
    let day = "";
    let hour = "";
    let groups = new Map();
    for (const row of ui.rows) {
      const d = dayLabel(row.at);
      if (d !== day) {
        day = d;
        hour = "";
        out.push(el("li", { className: "act-day", textContent: d }));
      }
      const h = String(new Date(row.at).getHours());
      if (h !== hour) {
        hour = h;
        groups = new Map();
      }
      const key = `${row.machine || ""}/${row.workspace_id || ""}`;
      let group = groups.get(key);
      if (!group) {
        const rows = el("ul", { className: "act-rows" });
        const head = el(
          "div",
          { className: "act-head" },
          el("span", { className: "act-ws", textContent: row.workspace || (row.kind === "hub.started" ? "hub" : "no workspace") }),
          row.machine ? el("span", { className: "act-machine", textContent: row.machine }) : null,
        );
        group = { rows };
        groups.set(key, group);
        out.push(el("li", { className: "act-group" }, head, rows));
      }
      group.rows.append(renderRow(row));
    }
    if (!ui.rows.length && !ui.loading) {
      out.push(el("li", { className: "act-empty", textContent: ui.error ? "" : "Nothing yet. Turns, reviews, fan-outs, and queue deliveries show up here as they happen." }));
    }
    if (ui.next) {
      out.push(el("li", { className: "act-more" }, el("button", {
        type: "button",
        className: "slab ghost",
        textContent: ui.loading ? "Loading..." : "Older",
        disabled: ui.loading,
        onclick: () => load(true),
      })));
    }
    list.replaceChildren(...out);
  }

  function renderStatus() {
    const bits = [];
    if (ui.loading) bits.push("loading...");
    if (ui.error) bits.push(ui.error);
    const missed = ui.machines.filter((m) => !m.ok);
    if (missed.length) bits.push(`partial: ${missed.map((m) => `${m.name} (${m.error})`).join(", ")}`);
    if (ui.since) bits.push(`since ${new Date(ui.since).toLocaleString()}`);
    const line = $("#act-status");
    line.textContent = bits.join(" · ");
    line.classList.toggle("bad", !!ui.error || missed.length > 0);
  }

  function renderFilters() {
    const where = (item) => (item.machine ? ` · ${item.machine}` : "");
    $("#act-ws").replaceChildren(
      el("option", { value: "", textContent: "all workspaces" }),
      ...state.workspaces.map((w) => el("option", { value: w.id, textContent: `${w.name}${where(w)}`, selected: w.id === ui.ws })),
    );
    const pool = state.sessions.filter((s) => !ui.ws || s.workspace_id === ui.ws);
    $("#act-session").replaceChildren(
      el("option", { value: "", textContent: "all sessions" }),
      ...pool.map((s) => el("option", { value: s.id, textContent: `${s.name}${where(s)}`, selected: s.id === ui.session })),
    );
    $("#act-kind").replaceChildren(...KINDS.map(([value, label]) => el("option", { value, textContent: label, selected: value === ui.kind })));
  }

  function renderPill() {
    const count = $("#act-count");
    if (!count) return;
    count.hidden = ui.unread === 0;
    count.textContent = ui.unread > 99 ? "99+" : String(ui.unread);
  }

  // --- open / close -------------------------------------------------------

  function open(options = {}) {
    if (ctx.onOpen) ctx.onOpen();
    document.body.classList.remove("side-open");
    ui.ws = options.ws || "";
    ui.session = options.session || "";
    ui.kind = options.kind || "";
    ui.since = options.since || "";
    ui.rows = [];
    ui.next = null;
    ui.unread = 0;
    renderPill();
    render();
    if (!dialog.open) dialog.showModal();
    load(false);
    loadSummary();
  }

  function close() {
    if (dialog.open) dialog.close();
  }

  // A pushed row (this hub's, or `machine`'s): on top when it matches the
  // filters. A row from a machine the view could not reach reloads it:
  // the machine is back, and what it missed is fetched.
  function onRow(pushed, machine) {
    if (!pushed || typeof pushed !== "object") return;
    const row = machine ? { ...pushed, machine } : pushed;
    if (!dialog.open) {
      if (row.kind !== "usage") {
        ui.unread += 1;
        renderPill();
      }
      return;
    }
    if (machine && ui.machines.some((m) => m.name === machine && !m.ok)) {
      load(false);
      loadSummary();
      return;
    }
    if (ui.ws && row.workspace_id !== ui.ws) return;
    if (ui.session && row.session_id !== ui.session) return;
    if (ui.kind && !ui.kind.split(",").some((k) => row.kind === k || row.kind.startsWith(`${k}.`))) return;
    if (ui.rows.some((r) => (r.machine || null) === (machine || null) && r.id === row.id)) return;
    ui.rows.unshift(row);
    markSeen([row]);
    render();
  }

  // A machine's events socket (re)opened: an open view that missed it
  // asks again.
  function onMachine(name) {
    if (dialog.open && ui.machines.some((m) => m.name === name && !m.ok)) {
      load(false);
      loadSummary();
    }
  }

  // --- while you were away ------------------------------------------------

  const card = $("#away");

  function since(iso) {
    const seconds = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
    if (seconds < 3600) return `${Math.round(seconds / 60)} min`;
    const h = Math.floor(seconds / 3600);
    const m = Math.round((seconds % 3600) / 60);
    return h < 48 ? `${h}h${m ? ` ${m}m` : ""}` : `${Math.floor(h / 24)} days`;
  }

  function stat(n, label, tone) {
    return el("div", { className: `away-stat${tone ? ` ${tone}` : ""}${n ? "" : " zero"}` }, el("b", { textContent: String(n) }), el("span", { textContent: label }));
  }

  function showDigest(digest) {
    const blocked = digest.blocked_now || [];
    const fanouts = digest.fanouts_graded || [];
    const cost = Number(digest.cost_usd || 0);
    const lines = [];
    for (const item of blocked.slice(0, 4)) {
      const session = sessionById(item.session_id);
      lines.push(el(
        "li",
        {},
        el("button", {
          type: "button",
          className: "away-link blocked",
          disabled: !session,
          onclick: () => {
            dismiss();
            ctx.openSession(item.session_id);
          },
        }, el("b", { textContent: item.session }), ` ${item.machine ? `on ${item.machine} ` : ""}waits${item.detail ? `: ${item.detail}` : ""}`),
      ));
    }
    for (const item of fanouts.slice(0, 3)) {
      const lead = item.winner ? `winner ${item.winner}` : item.leader ? `${item.leader} leads` : "none passing";
      lines.push(el(
        "li",
        {},
        el("button", {
          type: "button",
          className: "away-link",
          disabled: !!item.machine,
          onclick: () => {
            dismiss();
            ctx.openFanout(item.fanout_id);
          },
        }, el("b", { textContent: item.name }), ` graded: ${lead} (${item.passing}/${item.candidates} passing)`),
      ));
    }
    const missed = (digest.machines || []).filter((m) => !m.ok);
    card.replaceChildren(...[
      el(
        "header",
        { className: "away-head" },
        el("span", { className: "sheet-kicker", textContent: "while you were away" }),
        el("h3", { textContent: since(digest.since) }),
        el("button", { type: "button", className: "icon", title: "Dismiss", "aria-label": "Dismiss", onclick: () => dismiss() }, icon("x")),
      ),
      el(
        "div",
        { className: "away-stats" },
        stat(digest.turns_finished || 0, "turns finished"),
        stat(blocked.length, "blocked now", "bad"),
        stat(fanouts.length, "fan-outs graded"),
        stat(digest.commands_suggested || 0, "commands"),
        el("div", { className: `away-stat${cost ? "" : " zero"}` }, el("b", { textContent: window.FwHubFmt.dollars(cost) }), el("span", { textContent: "spent" })),
      ),
      lines.length ? el("ul", { className: "away-list" }, lines) : null,
      missed.length ? el("p", { className: "away-note", textContent: `Not included: ${missed.map((m) => m.name).join(", ")} (unreachable).` }) : null,
      el(
        "div",
        { className: "away-foot" },
        el("button", { type: "button", className: "slab", textContent: "Open activity", onclick: () => {
          dismiss();
          open({ since: digest.since });
        } }),
        el("button", { type: "button", className: "slab ghost", textContent: "Dismiss", onclick: () => dismiss() }),
      ),
    ].filter(Boolean));
    card.hidden = false;
  }

  function dismiss() {
    card.hidden = true;
    store.set("activity-away", null);
  }

  // The events socket's `away`: this browser is back after a while. The
  // span is kept until dismissed, so a reload shows the card again.
  async function onAway(message) {
    if (typeof message.since !== "string" || typeof message.until !== "string") return;
    store.set("activity-away", { since: message.since, until: message.until });
    await showAway(message);
  }

  async function showAway(span) {
    try {
      const p = new URLSearchParams({ since: span.since, until: span.until });
      const digest = await rawApi("GET", `/api/activity/digest?${p}`);
      if (digest.empty) store.set("activity-away", null);
      else showDigest(digest);
    } catch (error) {
      toast(`While you were away: ${error.message}`, { tone: "bad" });
    }
  }

  const pending = store.get("activity-away", null);
  if (pending && typeof pending.since === "string" && typeof pending.until === "string") showAway(pending);
  countUnread();

  // --- wiring -------------------------------------------------------------

  $("#act-ws").addEventListener("change", (event) => {
    ui.ws = event.target.value;
    ui.session = "";
    load(false);
    loadSummary();
  });
  $("#act-session").addEventListener("change", (event) => {
    ui.session = event.target.value;
    load(false);
    loadSummary();
  });
  $("#act-kind").addEventListener("change", (event) => {
    ui.kind = event.target.value;
    load(false);
  });
  $("#act-close").addEventListener("click", () => close());
  card.addEventListener("keydown", (event) => {
    if (event.key === "Escape") dismiss();
  });

  function paletteItems(cmd) {
    cmd("Activity", "what happened: turns, reviews, fan-outs, queues, on every machine", () => open(), "activity");
    const attached = sessionById(state.attached);
    if (attached) cmd(`Activity of ${attached.name}`, "this session only", () => open({ ws: attached.workspace_id, session: attached.id }), "activity");
    const workspace = attached && workspaceById(attached.workspace_id);
    if (workspace) cmd(`Activity in ${workspace.name}`, "this workspace only", () => open({ ws: workspace.id }), "activity");
    cmd("While you were away: last 8 hours", "the digest of the last 8 hours", async () => {
      try {
        const digest = await rawApi("GET", "/api/activity/digest?since=8h");
        if (digest.empty) toast("Nothing happened in the last 8 hours, and nobody is blocked.");
        else showDigest(digest);
      } catch (error) {
        toast(error.message, { tone: "bad" });
      }
    }, "activity");
  }

  return { open, close, isOpen: () => dialog.open, onRow, onAway, onMachine, paletteItems, refreshFilters: () => dialog.open && renderFilters() };
};

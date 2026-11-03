// flywheel hub: prompt queues. Per agent session, prompts the hub types in
// on its own, one finished turn at a time ("when it's done, then do X"):
// never while the agent is blocked on a prompt, someone types into it, or
// a fan-out grading or review action uses it. The hub that owns the
// session keeps the queue (GET/POST /api/sessions/{id}/queue, .../items/
// {item} PATCH/DELETE, .../move, .../pause|resume|clear); a machine's
// sessions are reached through this hub's proxy like every session call.
//
// Here: the drawer (a full-screen sheet on a phone) with the add form,
// the waiting items (drag to reorder on desktop, up/down buttons
// everywhere), edit and remove, pause/resume, and the delivered history
// with each delivery's time; the header pill and the row chip with the
// waiting count; Compose's "Queue" button; palette items; and stalled
// queues as Needs-you rows. app-base.js creates it with its helpers.
"use strict";

window.FwHubQueue = function (ctx) {
  const { $, el, icon, api, age, state, sessionById, isAgent } = ctx;
  const toast = (...args) => ctx.toast(...args);

  // How often an open drawer re-reads its queue (why the next one waits
  // changes with time: typing windows, settling).
  const POLL_MS = 2500;
  const NARROW = "(max-width: 760px)";
  const WHEN_LABEL = {
    done: "when done",
    idle: "done + read",
    "blocked-cleared": "after a block",
  };

  const drawer = $("#queue");
  const list = $("#q-list");
  const history = $("#q-history");
  const text = $("#q-text");
  const when = $("#q-when");
  const ui = {
    id: null, // the session whose queue is open
    view: null, // its last GET
    seq: 0,
    timer: 0,
    editing: null, // item id being edited
    editFor: null, // the session it belongs to
    editText: null, // { text, when } typed into the editor so far
    leasedAt: 0, // when the hub was last told it is open
    dragged: null, // item id being dragged
    briefs: new Map(), // session id -> last seen brief (JSON), to refetch on change
  };

  const brief = (session) => (session && session.queue) || null;
  const pendingOf = (session) => (brief(session) ? brief(session).pending || 0 : 0);
  const stalledOf = (session) => (brief(session) && brief(session).stalled) || null;
  const canQueue = (session) => isAgent(session);

  // --- the API ---------------------------------------------------------------

  async function load() {
    const id = ui.id;
    if (!id) return;
    const seq = ++ui.seq;
    try {
      const view = await api("GET", `/api/sessions/${id}/queue`);
      if (seq !== ui.seq || id !== ui.id) return;
      ui.view = view;
      renderDrawer();
    } catch (error) {
      if (seq !== ui.seq || id !== ui.id) return;
      $("#q-why").replaceChildren(el("span", { className: "q-bad", textContent: `Could not read the queue: ${error.message}` }));
    }
  }

  // Run `call` against the open queue; the answer is the new view.
  async function act(call, failure) {
    const id = ui.id;
    try {
      const view = await call(id);
      if (id === ui.id && view && view.items) {
        ui.view = view;
        renderDrawer();
      }
      ctx.refresh();
      return view;
    } catch (error) {
      toast(`${failure}: ${error.message}`, { tone: "bad", timeout: 7000 });
      return null;
    }
  }

  // Queue `prompt` for session `id` (Compose's "Queue", the drawer's form).
  async function add(id, prompt, trigger) {
    const session = sessionById(id);
    if (!canQueue(session)) {
      toast("Prompts queue for Claude Code and Codex sessions only.", { tone: "bad" });
      return null;
    }
    const answer = await api("POST", `/api/sessions/${id}/queue`, { text: prompt, when: trigger || "done" });
    if (ui.id === id) {
      ui.view = answer.queue;
      renderDrawer();
    }
    ctx.refresh();
    const why = answer.queue.waiting;
    toast(`Queued for ${session.name} (${answer.queue.pending} waiting)${why ? `: ${why}` : ""}.`, {
      timeout: 4500,
      actions: ui.id === id
        ? []
        : [el("button", { type: "button", className: "toast-link", textContent: "Open queue", onclick: () => open(id) })],
    });
    return answer;
  }

  const itemPath = (id, item) => `/api/sessions/${id}/queue/items/${item}`;

  // An open editor keeps its item back on the hub (a lease renewed while
  // it stays open), so the trigger cannot send the text being changed.
  function lease(id, item, editing) {
    if (!id || !item) return;
    if (editing) ui.leasedAt = Date.now();
    api("POST", `${itemPath(id, item)}/editing`, { editing }).catch(() => {});
  }

  function startEdit(item) {
    if (ui.editing && ui.editing !== item.id) lease(ui.editFor, ui.editing, false);
    ui.editing = item.id;
    ui.editFor = ui.id;
    ui.editText = null;
    lease(ui.id, item.id, true);
    renderDrawer();
  }

  function stopEdit() {
    if (ui.editing) lease(ui.editFor, ui.editing, false);
    ui.editing = null;
    ui.editFor = null;
    ui.editText = null;
  }
  const move = (item, position) =>
    act((id) => api("POST", `${itemPath(id, item)}/move`, { position }), "Not moved");
  const remove = (item) => act((id) => api("DELETE", itemPath(id, item)), "Not removed");
  const setPaused = (paused, id = ui.id) =>
    act(() => api("POST", `/api/sessions/${id}/queue/${paused ? "pause" : "resume"}`, {}), paused ? "Not paused" : "Not resumed");

  // --- the drawer ------------------------------------------------------------

  function isOpen() {
    return !!ui.id;
  }

  function open(id, focusAdd) {
    const session = sessionById(id);
    if (!canQueue(session)) {
      toast("Prompts queue for Claude Code and Codex sessions only.", { tone: "bad" });
      return;
    }
    ctx.onOpen();
    if (ui.id !== id) {
      stopEdit();
      ui.id = id;
      ui.view = null;
      $("#q-sent").open = false;
    }
    drawer.hidden = false;
    renderDrawer();
    renderPill();
    load();
    clearInterval(ui.timer);
    ui.timer = setInterval(() => {
      if (ui.editing && Date.now() - ui.leasedAt > 10000) lease(ui.editFor, ui.editing, true);
      if (document.visibilityState === "visible") load();
    }, POLL_MS);
    if (focusAdd || !window.matchMedia(NARROW).matches) text.focus();
    else drawer.focus();
  }

  function close() {
    if (!ui.id) return;
    stopEdit();
    ui.id = null;
    ui.view = null;
    ui.seq++;
    clearInterval(ui.timer);
    drawer.hidden = true;
    renderPill();
    ctx.focusTerminal();
  }

  function togglePill() {
    const id = state.attached;
    if (ui.id && ui.id === id) close();
    else if (id && canQueue(sessionById(id))) open(id);
    else return false;
    return true;
  }

  function statusLine(view) {
    const out = [];
    if (view.stalled) {
      out.push(
        el(
          "div",
          { className: "q-stall", role: "alert" },
          el("b", { textContent: "Stalled" }),
          el("span", { textContent: ` ${age(view.stalled.at)} ago: ${view.stalled.reason}. Nothing more is sent until you resume.` }),
          el("button", { type: "button", className: "slab", textContent: "Resume", onclick: () => setPaused(false) }),
        ),
      );
    } else if (view.paused) {
      out.push(
        el(
          "div",
          { className: "q-paused" },
          el("span", { textContent: "Paused: nothing is sent." }),
          el("button", { type: "button", className: "slab ghost", textContent: "Resume", onclick: () => setPaused(false) }),
        ),
      );
    } else if (view.pending && view.waiting) {
      out.push(el("span", { className: "q-next" }, el("b", { textContent: "next " }), view.waiting));
    } else if (!view.pending) {
      out.push(el("span", { className: "q-next quiet", textContent: "Nothing waiting. Queue a prompt: it is typed in once the agent's turn is done." }));
    }
    return out;
  }

  function editForm(item) {
    const kept = ui.editText;
    const area = el("textarea", { className: "q-edit-text", value: kept ? kept.text : item.text, rows: 3, "aria-label": "Prompt" });
    const pick = el(
      "select",
      { className: "q-edit-when", "aria-label": "When to send it" },
      ...Object.keys(WHEN_LABEL).map((w) => el("option", { value: w, textContent: WHEN_LABEL[w], selected: w === (kept ? kept.when : item.when) })),
    );
    const keep = () => { ui.editText = { text: area.value, when: pick.value }; };
    area.addEventListener("input", keep);
    pick.addEventListener("change", keep);
    const save = async () => {
      const done = await act(
        (id) => api("PATCH", itemPath(id, item.id), { text: area.value, when: pick.value }),
        "Not saved",
      );
      if (done) {
        stopEdit();
        renderDrawer();
      }
    };
    area.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
        event.preventDefault();
        save();
      } else if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        stopEdit();
        renderDrawer();
      }
    });
    setTimeout(() => area.focus(), 0);
    return el(
      "div",
      { className: "q-edit" },
      area,
      el(
        "div",
        { className: "q-edit-row" },
        pick,
        el("button", { type: "button", className: "slab ghost", textContent: "Cancel", onclick: () => { stopEdit(); renderDrawer(); } }),
        el("button", { type: "button", className: "slab", textContent: "Save", onclick: save }),
      ),
    );
  }

  function pendingRow(item, count) {
    const sending = item.state === "sending";
    const editing = ui.editing === item.id;
    const desktop = !window.matchMedia(NARROW).matches;
    const pos = item.position || 0;
    const row = el(
      "li",
      {
        className: `q-item${sending ? " sending" : ""}${pos === 1 ? " head" : ""}`,
        dataset: { id: item.id, pos: String(pos) },
        draggable: desktop && !sending && !editing,
        ondragstart: (event) => {
          ui.dragged = item.id;
          event.dataTransfer.effectAllowed = "move";
          event.dataTransfer.setData("text/plain", item.text);
          row.classList.add("dragging");
        },
        ondragend: () => {
          ui.dragged = null;
          row.classList.remove("dragging");
          for (const n of list.querySelectorAll(".drop-before, .drop-after")) n.classList.remove("drop-before", "drop-after");
        },
      },
      el(
        "span",
        { className: "q-pos", title: sending ? "being typed in now" : `number ${pos} in line` },
        desktop && !sending ? icon("grip") : null,
        el("b", { textContent: sending ? "..." : String(pos) }),
      ),
      el(
        "div",
        { className: "q-body" },
        el(
          "div",
          { className: "q-meta" },
          el("span", { className: `q-when ${item.when}`, textContent: WHEN_LABEL[item.when] || item.when }),
          el("span", { className: "q-age", textContent: sending ? "sending" : `queued ${age(item.added_at)} ago` }),
        ),
        editing
          ? editForm(item)
          : el("p", {
            className: "q-text",
            textContent: item.text,
            title: "Click to edit",
            onclick: () => {
              if (sending) return;
              startEdit(item);
            },
          }),
      ),
      sending || editing
        ? null
        : el(
          "span",
          { className: "q-actions" },
          el("button", { type: "button", className: "icon", title: "Sooner", "aria-label": "Move up", disabled: pos <= 1, onclick: () => move(item.id, pos - 1) }, "↑"),
          el("button", { type: "button", className: "icon", title: "Later", "aria-label": "Move down", disabled: pos >= count, onclick: () => move(item.id, pos + 1) }, "↓"),
          el("button", { type: "button", className: "icon", title: "Edit", "aria-label": "Edit", onclick: () => startEdit(item) }, icon("pen")),
          el("button", { type: "button", className: "icon danger", title: "Remove", "aria-label": "Remove", onclick: () => remove(item.id) }, icon("x")),
        ),
    );
    return row;
  }

  function historyRow(item) {
    const at = item.delivered_at;
    const clock = at ? new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" }) : "";
    return el(
      "li",
      { className: "q-done", title: at ? new Date(at).toLocaleString() : "" },
      el("span", { className: "q-done-at", textContent: clock }),
      el("span", { className: "q-done-text", textContent: item.text }),
      item.reconciled
        ? el("span", { className: "q-done-note", textContent: "found in its transcript after a restart: not typed again" })
        : null,
      el("button", { type: "button", className: "icon", title: "Queue it again", "aria-label": "Queue it again", onclick: () => add(ui.id, item.text, item.when) }, icon("redo")),
    );
  }

  function renderDrawer() {
    if (!ui.id) {
      drawer.hidden = true;
      return;
    }
    const session = sessionById(ui.id);
    if (!session) {
      close();
      return;
    }
    $("#q-title").textContent = session.name;
    const view = ui.view;
    const stateText = !view ? "" : view.stalled ? "stalled" : view.paused ? "paused" : view.pending ? `${view.pending} waiting` : "";
    $("#q-state").textContent = stateText;
    $("#q-state").dataset.tone = !view ? "" : view.stalled ? "bad" : view.paused ? "warn" : "";
    const pause = $("#q-pause");
    pause.textContent = view && view.paused ? "Resume" : "Pause";
    pause.disabled = !view;
    if (!view) {
      $("#q-why").replaceChildren(el("span", { className: "q-next quiet", textContent: "Reading the queue..." }));
      return;
    }
    $("#q-why").replaceChildren(...statusLine(view));
    const waiting = view.items.filter((i) => i.state !== "delivered");
    const delivered = view.items.filter((i) => i.state === "delivered").reverse();
    // The item being edited went (sent before the edit was saved, or
    // removed elsewhere): say so, and keep the edit to queue as new.
    if (ui.editing && !waiting.some((i) => i.id === ui.editing && i.state === "pending")) {
      const was = view.items.find((i) => i.id === ui.editing);
      const edit = ui.editText;
      const changed = !!edit && (!was || edit.text !== was.text || edit.when !== was.when);
      const sessionId = ui.id;
      stopEdit();
      const what = was && was.state !== "pending" ? "was sent before your edit was saved" : "is no longer queued";
      toast(`That prompt ${what}.${changed ? " Your edit was not sent." : ""}`, {
        tone: "bad",
        timeout: 12000,
        actions: changed && edit.text.trim()
          ? [el("button", { type: "button", className: "toast-link", textContent: "Queue the edit", onclick: () => add(sessionId, edit.text, edit.when).catch((e) => toast(`Not queued: ${e.message}`, { tone: "bad" })) })]
          : [],
      });
    }
    // A poll never redraws an open editor under the user's fingers (its
    // text would go); the list catches up once it is saved or cancelled.
    const active = document.activeElement;
    const typingHere = ui.editing && active && list.contains(active) && !!active.closest(".q-edit") &&
      waiting.some((i) => i.id === ui.editing && i.state === "pending");
    if (!typingHere) list.replaceChildren(...waiting.map((item) => pendingRow(item, waiting.length)));
    $("#q-sent").hidden = !delivered.length;
    $("#q-sent-sum").textContent = `sent (${delivered.length})${view.last_delivered_at ? `, last ${age(view.last_delivered_at)} ago` : ""}`;
    history.replaceChildren(...delivered.map(historyRow));
    $("#q-clear").hidden = !waiting.some((i) => i.state === "pending");
    $("#q-hint").textContent = `Not sent while someone typed in the last ${view.typing_quiet_secs}s.`;
  }

  // Desktop: drop a dragged item before or after the row under it.
  list.addEventListener("dragover", (event) => {
    if (!ui.dragged) return;
    const row = event.target.closest(".q-item");
    if (!row || row.dataset.id === ui.dragged || row.classList.contains("sending")) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
    const box = row.getBoundingClientRect();
    const after = event.clientY > box.top + box.height / 2;
    for (const n of list.querySelectorAll(".drop-before, .drop-after")) n.classList.remove("drop-before", "drop-after");
    row.classList.add(after ? "drop-after" : "drop-before");
  });
  list.addEventListener("drop", (event) => {
    const dragged = ui.dragged;
    const row = event.target.closest(".q-item");
    if (!dragged || !row || row.dataset.id === dragged) return;
    event.preventDefault();
    const items = (ui.view ? ui.view.items : []).filter((i) => i.state !== "delivered");
    const from = items.find((i) => i.id === dragged);
    const target = Number(row.dataset.pos);
    if (!from || !target) return;
    const after = row.classList.contains("drop-after");
    // The position among the waiting items once the dragged one is out.
    let position = target + (after ? 1 : 0);
    if (from.position < position) position -= 1;
    if (position !== from.position) move(dragged, Math.max(1, position));
  });

  $("#q-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const id = ui.id;
    const prompt = text.value;
    if (!id || !prompt.trim()) {
      text.focus();
      return;
    }
    const button = $("#q-add");
    button.disabled = true;
    try {
      await add(id, prompt, when.value);
      text.value = "";
    } catch (error) {
      toast(`Not queued: ${error.message}`, { tone: "bad", timeout: 7000 });
    } finally {
      button.disabled = false;
      text.focus();
    }
  });
  text.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      $("#q-form").requestSubmit();
    }
  });
  $("#q-pause").addEventListener("click", () => {
    if (ui.view) setPaused(!ui.view.paused);
  });
  $("#q-clear").addEventListener("click", async () => {
    const session = sessionById(ui.id);
    const ok = await ctx.confirmDialog({
      kicker: "queue",
      title: "Clear the queue?",
      body: `Every prompt still waiting for ${session ? session.name : "this session"} is removed. Sent ones stay in the history.`,
      ok: "Clear",
    });
    if (ok) act((id) => api("POST", `/api/sessions/${id}/queue/clear`, {}), "Not cleared");
  });
  $("#q-close").addEventListener("click", close);
  drawer.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !document.querySelector("dialog[open]")) {
      event.stopPropagation();
      close();
    }
  });

  // --- the header, rows, palette, Needs you ------------------------------------

  function renderPill() {
    const pill = $("#queue-toggle");
    const session = sessionById(state.attached);
    pill.hidden = !canQueue(session);
    if (pill.hidden) return;
    const pending = pendingOf(session);
    const stalled = stalledOf(session);
    const count = $("#queue-count");
    count.hidden = !pending && !stalled;
    count.textContent = stalled ? "!" : String(pending);
    pill.dataset.tone = stalled ? "bad" : brief(session) && brief(session).paused ? "warn" : "";
    pill.setAttribute("aria-pressed", ui.id === session.id ? "true" : "false");
  }

  // Called on every render: the pill, and an open drawer re-read when its
  // session's queue changed on the hub.
  function renderHeader() {
    renderPill();
    for (const session of state.sessions) {
      const key = JSON.stringify(brief(session));
      const before = ui.briefs.get(session.id);
      ui.briefs.set(session.id, key);
      if (session.id === ui.id && before !== undefined && before !== key) load();
    }
    if (ui.id && !sessionById(ui.id)) close();
  }

  function rowChip(session) {
    const queued = brief(session);
    if (!queued || (!queued.pending && !queued.stalled)) return null;
    const stalled = queued.stalled;
    return el(
      "button",
      {
        type: "button",
        className: `q-chip${stalled ? " stalled" : ""}${queued.paused ? " paused" : ""}`,
        title: stalled
          ? `queue stalled: ${stalled.reason}`
          : `${queued.pending} prompt${queued.pending === 1 ? "" : "s"} queued${queued.paused ? " (paused)" : ""}`,
        "aria-label": stalled ? "queue stalled" : `${queued.pending} queued`,
        onclick: async (event) => {
          event.stopPropagation();
          await ctx.openSession(session.id);
          open(session.id);
        },
      },
      icon("queue"),
      el("span", { textContent: stalled ? "stalled" : String(queued.pending) }),
    );
  }

  function paletteItems(cmd) {
    const session = sessionById(state.attached);
    if (canQueue(session)) {
      cmd(`Queue a prompt for ${session.name}`, "typed in once its turn is done", () => open(session.id, true), "queue");
      cmd(`Prompt queue: ${session.name}`, `${ctx.keyOf("queue.toggle")}${pendingOf(session) ? ` · ${pendingOf(session)} waiting` : ""}`, () => open(session.id), "queue");
      const queued = brief(session);
      const paused = !!(queued && queued.paused);
      cmd(`${paused ? "Resume" : "Pause"} the prompt queue of ${session.name}`, paused ? "deliver again" : "deliver nothing until resumed", () => setPaused(!paused, session.id), "queue");
    }
    for (const other of state.sessions) {
      if (stalledOf(other)) {
        cmd(`Resume the stalled queue of ${other.name}`, stalledOf(other).reason, () => setPaused(false, other.id), "queue");
      }
    }
  }

  // Sessions whose queue stalled (Needs you).
  function attention() {
    return state.sessions.filter((s) => stalledOf(s));
  }

  function attentionRow(session) {
    const stall = stalledOf(session);
    return el(
      "li",
      {},
      el(
        "button",
        {
          type: "button",
          className: "attn-row queue-stalled",
          onclick: async () => {
            await ctx.openSession(session.id);
            open(session.id);
          },
        },
        el("span", { className: "glyph queue" }, icon("queue")),
        el(
          "span",
          { className: "attn-text" },
          el(
            "span",
            { className: "attn-line" },
            el("b", { className: "attn-name", textContent: session.name }),
            session.machine ? el("span", { className: "machine-badge", textContent: session.machine }) : null,
            el("span", { className: "attn-ws", textContent: `${pendingOf(session)} queued` }),
          ),
          el("span", { className: "attn-detail", textContent: `queue stalled: ${stall.reason}` }),
        ),
        el(
          "span",
          { className: "attn-meta" },
          el("span", { className: "attn-status blocked", textContent: "stalled" }),
          el("span", { className: "attn-age", textContent: age(stall.at) }),
        ),
      ),
    );
  }

  return { open, close, isOpen, togglePill, add, renderHeader, rowChip, paletteItems, attention, attentionRow, canQueue };
};

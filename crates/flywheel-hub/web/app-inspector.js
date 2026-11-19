// flywheel hub web client: the session inspector. A card beside a
// sidebar row with everything the row leaves out (a row is a dot, a name,
// and one fact): what it is doing, its plan, context, cost, where it
// runs, its account, who started it and what it started, its queue, PR,
// review pair, a pending relaunch or end. It floats (fixed, beside the
// sidebar), so no row moves for it. Opened by hovering a row, by I on a
// focused row, or by a row's Details button (touch: the attached row's);
// on a phone it is a sheet. `fw hub ls --filter NAME` and `fw hub fleet
// NAME` print the same facts.
"use strict";

const inspector = (() => {
  const card = el("aside", { className: "why-pop inspector", id: "inspector", role: "dialog", "aria-label": "Session details" });
  card.hidden = true;
  document.body.append(card);
  let shownId = null;
  let anchor = null;
  let pinned = false;
  let showTimer = 0;
  let hideTimer = 0;
  // A pointer resting on a row this long opens its card; one moving on
  // from a card's row to another row swaps it at once.
  const HOVER_MS = 450;
  const LEAVE_MS = 220;
  const hovers = () => window.matchMedia("(hover: hover)").matches;

  const rowOf = (id) => {
    const key = CSS.escape(id);
    return document.querySelector(`#side-scroll li.session[data-id="${key}"], #side-scroll .attn-row[data-id="${key}"]`);
  };

  function row(key, ...value) {
    const kept = value.flat().filter((v) => v !== null && v !== undefined && v !== false && v !== "");
    if (!kept.length) return null;
    return el("div", { className: "why-row", dataset: { wk: key } }, el("span", { className: "why-key", textContent: key }), el("span", { className: "why-val" }, kept));
  }
  const muted = (text) => (text ? el("span", { className: "muted", textContent: text }) : null);
  const nameOf = (id) => (sessionById(id) || {}).name || String(id || "").slice(0, 8);

  // What it does now: its state, then the step or what it waits on.
  function nowLine(session, st) {
    const agent = agentStatus(session);
    const doing = agent ? doingOf(session, agent) : null;
    const t = sessionTelemetry.get(session.id);
    const parts = [el("span", { className: `dot ${st.dot}`, "aria-hidden": "true" }), " ", el("b", { textContent: st.label })];
    if (doing) parts.push(" ", muted(`${[doing.what, doing.count, doing.more].filter(Boolean).join(" ")}${doing.age ? ` ${doing.age}` : ""}`));
    else if (st.detail) parts.push(" ", muted(st.detail));
    else if (agent && agent.state === "failed" && t && t.last_error) parts.push(" ", muted(t.last_error));
    else if (st.title && st.title !== st.label) parts.push(" ", muted(st.title));
    return parts;
  }

  function planLine(session) {
    const t = sessionTelemetry.get(session.id);
    if (!t || !t.plan || !t.plan.total) return null;
    const bar = el("span", { className: "prog-bar" }, el("i"));
    bar.firstChild.style.width = `${Math.round((t.plan.done / t.plan.total) * 100)}%`;
    return [bar, el("b", { textContent: ` ${t.plan.done}/${t.plan.total}` }), " ", muted(t.plan.current || "")];
  }

  function contextLine(session) {
    const t = sessionTelemetry.get(session.id);
    if (!t || !t.context) return null;
    const pct = Math.round(t.context.pct);
    return el("span", { className: pct >= CTX_WARN ? "hot" : "", textContent: `${t.context.exact === false ? "~" : ""}${pct}% used` });
  }

  // Started by, companion of, the sessions it started (live), its review pair.
  function familyLine(session) {
    const out = [];
    if (session.opened_by) out.push(`started by ${nameOf(session.opened_by)}`);
    if (session.companion_of) out.push(`companion shell of ${nameOf(session.companion_of)}`);
    const kids = state.sessions.filter((s) => s.opened_by === session.id && !endedSession(s));
    if (kids.length) out.push(`started ${kids.length} running: ${kids.map((s) => s.name).join(", ")}`);
    const companion = state.sessions.find((s) => s.companion_of === session.id && !endedSession(s));
    if (companion) out.push(`shell ${companion.name}`);
    return out.length ? out.join("; ") : null;
  }

  // A process from before the hub's last upgrade: relaunched at its next rest.
  function relaunchLine(session) {
    const s = session.stale;
    if (!s || session.state !== "running") return null;
    if (s.held) return `held: ${s.held} (from flywheel ${s.from}; relaunch it yourself)`;
    return `${s.hung_up_at ? "relaunching" : "pending"} (from flywheel ${s.from})${s.waits ? `; waits: ${s.waits}` : ""}`;
  }

  // An end in progress, or how it ended.
  function endLine(session) {
    const byName = (by) => (by && by.kind === "session" ? nameOf(by.id) : "you");
    if (session.ending) {
      const e = session.ending;
      return `${e.replace ? `replacing with ${e.replace.name}` : "ending"} (by ${byName(e.by)}), ${e.phase.replace(/_/g, " ")}`;
    }
    return endedHow(session);
  }

  // Leases it holds (resource, mode, time left) and waits for.
  function leasesLine(session) {
    const view = session.leases;
    if (!view) return "";
    const now = Date.now();
    const mins = (ms) => `${Math.max(0, Math.round(ms / 60000))}m`;
    const held = (view.leases || []).map((l) =>
      `${l.resource} ${l.mode === "shared" ? "shared" : "excl"} ${mins(l.expires_at - now)} left`);
    const waits = (view.waiters || []).map((w) => `waiting ${w.resource} ${mins(now - w.queued_at)}`);
    return [...held, ...waits].join(", ");
  }

  // Its labels (key=value): the operator's first, then the hub's fw.*
  // keys a row above does not already say (fw.kind: the kind tag;
  // fw.parent: family; fw.change: branch; fw.pr: pr; fw.state: now). A
  // value naming a session reads as its name.
  const SHOWN_ELSEWHERE = new Set(["fw.kind", "fw.parent", "fw.change", "fw.pr", "fw.state"]);
  function labelsLine(session) {
    const labels = Object.entries(session.labels || {})
      .filter(([k]) => !SHOWN_ELSEWHERE.has(k))
      .sort(([a], [b]) => (a.startsWith("fw.") - b.startsWith("fw.")) || a.localeCompare(b));
    return labels.map(([k, v]) => {
      const named = sessionById(String(v));
      return el("span", { className: "insp-label", textContent: `${k}=${named ? named.name : v}`, title: named ? `${k}=${v}` : undefined });
    });
  }

  // Its actions (the row's, which a desktop row shows only to the
  // keyboard): rename, stop or relaunch, close.
  function actions(session) {
    const act = (label, title, run, danger) => el("button", {
      type: "button",
      className: `chip${danger ? " danger" : ""}`,
      textContent: label,
      title,
      "aria-label": title,
      onclick: () => {
        close();
        run();
      },
    });
    return el("div", { className: "insp-acts" },
      act("rename", "Rename (F2)", () => startRename(session.id)),
      session.state === "running" && session.live
        ? act("stop", "Stop: hang up, keep the scrollback", () => stopSession(session.id))
        : act("relaunch", "Relaunch in place", () => relaunch(session.id)),
      act("close", "Close session", () => closeSession(session.id), true));
  }

  function content(session) {
    const st = statusLook(session);
    const ws = workspaceById(session.workspace_id);
    const folder = session.cwd ? sessionFolder(session) : null;
    return [
      el("div", { className: "why-head" },
        el("b", { className: "insp-name", textContent: session.name }),
        el("span", { className: `kind-tag ${session.kind}`, textContent: window.FwHubFmt.kindLabel(session) }),
        el("span", { className: "why-title" }),
        el("button", { type: "button", className: "why-close", "aria-label": "Close", onclick: () => close() }, "×")),
      row("now", nowLine(session, st)),
      row("plan", planLine(session)),
      row("context", contextLine(session)),
      row("cost", launcher.rowChip(session)),
      row("where", ws ? ws.name : "", folder ? [" ", muted("/"), " ", folder.label] : null),
      row("branch", worktrees.badge(session)),
      row("account", session.account ? `@${session.account}` : ""),
      row("machine", session.machine || ""),
      row("family", familyLine(session)),
      row("review", handoff.pairChip(session)),
      row("queue", promptQueue.rowChip(session)),
      row("leases", leasesLine(session)),
      row("pr", pullRequests.rowChip(session)),
      row("relaunch", relaunchLine(session)),
      row("end", endLine(session)),
      row("recording", session.recording || ""),
      row("labels", labelsLine(session)),
      actions(session),
      el("p", { className: "why-note insp-cli", textContent: `fw hub ls --filter ${/\s/.test(session.name) ? JSON.stringify(session.name) : session.name}` }),
    ];
  }

  function place() {
    if (window.matchMedia(NARROW).matches) {
      card.classList.add("sheet");
      card.style.left = "";
      card.style.top = "";
      return;
    }
    card.classList.remove("sheet");
    const side = $("#sidebar").getBoundingClientRect();
    const at = anchor && anchor.isConnected ? anchor.getBoundingClientRect() : null;
    const top = Math.max(8, Math.min((at ? at.top : side.top) - 8, window.innerHeight - card.offsetHeight - 8));
    card.style.left = `${Math.round(side.right + 6)}px`;
    card.style.top = `${Math.round(top)}px`;
  }

  function draw() {
    const session = sessionById(shownId);
    if (!session) {
      close();
      return;
    }
    if (!anchor || !anchor.isConnected) anchor = rowOf(shownId);
    morph(card, content(session));
    place();
  }

  function open(id, node, pin) {
    clearTimeout(showTimer);
    clearTimeout(hideTimer);
    shownId = id;
    anchor = node || rowOf(id);
    pinned = !!pin;
    card.hidden = false;
    draw();
  }

  function close() {
    clearTimeout(showTimer);
    clearTimeout(hideTimer);
    if (card.hidden) return;
    card.hidden = true;
    shownId = null;
    anchor = null;
    pinned = false;
  }

  // A pointer over a row (mouse only: touch has no hover).
  function hover(id, node) {
    if (!hovers()) return;
    clearTimeout(hideTimer);
    clearTimeout(showTimer);
    if (pinned && shownId !== id) return;
    if (!card.hidden && shownId === id) return;
    showTimer = setTimeout(() => open(id, node, false), card.hidden ? HOVER_MS : 60);
  }

  function leave() {
    clearTimeout(showTimer);
    if (pinned || card.hidden) return;
    hideTimer = setTimeout(close, LEAVE_MS);
  }

  function toggle(id, node) {
    if (!card.hidden && shownId === id) close();
    else open(id, node, true);
  }

  card.addEventListener("mouseenter", () => clearTimeout(hideTimer));
  card.addEventListener("mouseleave", () => leave());
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !card.hidden) close();
  });
  document.addEventListener("pointerdown", (event) => {
    if (card.hidden || card.contains(event.target)) return;
    // A press on another row opens that session: the card goes with it.
    if (pinned && event.target.closest && event.target.closest(".session-actions")) return;
    close();
  });
  $("#side-scroll").addEventListener("scroll", () => {
    if (!card.hidden) place();
  }, { passive: true });
  window.addEventListener("resize", () => {
    if (!card.hidden) place();
  });

  return {
    hover,
    leave,
    toggle,
    close,
    isOpen: () => !card.hidden,
    shown: () => shownId,
    // After a repaint: the open card follows its session's facts and row.
    refresh: () => {
      if (!card.hidden) draw();
    },
  };
})();

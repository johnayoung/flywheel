// flywheel hub: the live map of one agent session -- what it is doing,
// drawn instead of listed. A panel at the top of the Work drawer (beside
// the terminal, never instead of it), folded away with its toggle.
//
// The session sits in the centre, its context window as the ring around
// it (amber at 70%, red at 90%). Around it, the stations a call goes to by
// the kind of work it is (the hub tags each call: telemetry/station.rs):
// you, reading, editing, shell, waiting, network, browser, agents, tools.
// A call sends a pulse out along its spoke when it starts and back when it
// ends; a station stays lit while a call there runs, with two clock hands
// growing round it (one a minute, one ten) and `xN` for calls at once; a
// failed or refused call leaves a red count until the next prompt. A spoke
// thickens the more its station is used; a thin line joins the last few
// stations the main agent went through, each leg fading after 40 seconds.
// Fed by work.js with the session's events, oldest first.
"use strict";

window.FwHubMap = function () {
  const NS = "http://www.w3.org/2000/svg";
  const W = 440;
  const H = 236;
  const CX = W / 2;
  const CY = H / 2 + 4;
  const RX = 172;
  const RY = 88;
  // Clockwise from the top.
  const STATIONS = [
    ["you", "you"],
    ["network", "network"],
    ["browser", "browser"],
    ["agents", "agents"],
    ["tools", "tools"],
    ["wait", "waiting"],
    ["shell", "shell"],
    ["edits", "editing"],
    ["files", "reading"],
  ];
  // The hub's finer kinds the map shows together.
  const FOLD = { skills: "tools", plan: "tools", memory: "tools", other: "tools" };
  const ROUTE_LEGS = 6;
  const LEG_HOLD = 40_000;
  const LEG_FADE = 20_000;
  const PULSE_MS = 520;
  const still = () => window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  const svg = (tag, attrs, ...kids) => {
    const node = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs || {})) node.setAttribute(k, String(v));
    for (const kid of kids) if (kid) node.append(kid);
    return node;
  };

  const place = new Map(STATIONS.map(([id], i) => {
    const a = -Math.PI / 2 + (i / STATIONS.length) * Math.PI * 2;
    return [id, { x: CX + RX * Math.cos(a), y: CY + RY * Math.sin(a) }];
  }));

  // --- state ---------------------------------------------------------------

  let s;
  function fresh() {
    return {
      calls: new Map(), // station -> calls seen
      open: new Map(), // call id -> { station, since }
      errors: new Map(), // station -> failures since the prompt
      route: [], // { station, at } of the main agent
      subagents: new Set(),
      waiting: false, // on a person
      compactAt: 0,
      context: null,
      status: "",
      now: Date.now(),
    };
  }
  s = fresh();

  // --- the drawing ----------------------------------------------------------

  const root = svg("svg", { class: "mp-svg", viewBox: `0 0 ${W} ${H}`, role: "img", "aria-label": "Live map of the session" });
  const spokes = svg("g", { class: "mp-spokes" });
  const routeG = svg("g", { class: "mp-route" });
  const pulses = svg("g", { class: "mp-pulses" });
  const stationsG = svg("g", { class: "mp-stations" });
  const ring = svg("circle", { class: "mp-ring", cx: CX, cy: CY, r: 34, pathLength: 100, transform: `rotate(-90 ${CX} ${CY})` });
  const pctText = svg("text", { class: "mp-pct", x: CX, y: CY + 1 });
  const statusText = svg("text", { class: "mp-status", x: CX, y: CY + 14 });
  root.append(
    spokes,
    routeG,
    svg("circle", { class: "mp-ring-bed", cx: CX, cy: CY, r: 34 }),
    ring,
    svg("circle", { class: "mp-core", cx: CX, cy: CY, r: 28 }),
    pctText,
    statusText,
    pulses,
    stationsG,
  );

  const parts = new Map();
  for (const [id, label] of STATIONS) {
    const { x, y } = place.get(id);
    const spoke = svg("line", { class: "mp-spoke", x1: CX, y1: CY, x2: x, y2: y });
    spokes.append(spoke);
    const inner = svg("circle", { class: "mp-hand", cx: x, cy: y, r: 16, pathLength: 100, transform: `rotate(-90 ${x} ${y})` });
    const outer = svg("circle", { class: "mp-hand outer", cx: x, cy: y, r: 19.5, pathLength: 100, transform: `rotate(-90 ${x} ${y})` });
    const dot = svg("circle", { class: `mp-node st-${id === "tools" ? "other" : id}`, cx: x, cy: y, r: 12 });
    const count = svg("text", { class: "mp-count", x, y: y + 3.5 });
    const below = y > CY + 10;
    const name = svg("text", { class: "mp-label", x, y: below ? y + 31 : y - 25 });
    name.textContent = label;
    const many = svg("text", { class: "mp-many", x: x + 17, y: y - 12 });
    const err = svg("g", { class: "mp-err", transform: `translate(${x - 14} ${y - 13})` },
      svg("circle", { r: 6 }), svg("text", { y: 3 }));
    const g = svg("g", { class: "mp-st", "data-station": id }, inner, outer, dot, count, name, many, err);
    const title = svg("title", {});
    g.append(title);
    stationsG.append(g);
    parts.set(id, { g, spoke, inner, outer, count, many, err, title });
  }

  const node = document.createElement("div");
  node.className = "mp";
  node.append(root);

  // --- feeding -------------------------------------------------------------

  const stationOf = (event) => {
    const raw = event.station || "other";
    return FOLD[raw] || raw;
  };

  function pulse(station, back) {
    if (still() || !node.isConnected) return;
    const to = place.get(station);
    if (!to) return;
    const dot = svg("circle", { class: `mp-pulse${back ? " back" : ""}`, r: 3.2, cx: CX, cy: CY });
    const from = back ? to : { x: CX, y: CY };
    const end = back ? { x: CX, y: CY } : to;
    dot.setAttribute("cx", from.x);
    dot.setAttribute("cy", from.y);
    pulses.append(dot);
    const anim = dot.animate(
      [{ transform: "translate(0,0)" }, { transform: `translate(${end.x - from.x}px,${end.y - from.y}px)` }],
      { duration: PULSE_MS, easing: "ease-out" },
    );
    anim.onfinish = () => dot.remove();
  }

  // `events` oldest first; `live`: they just happened (pulse them).
  function feed(events, live) {
    for (const event of events) {
      const body = event.body || {};
      const main = !event.agent;
      switch (event.kind) {
        case "turn.started":
          s.errors.clear();
          s.waiting = false;
          break;
        case "tool.started": {
          const station = stationOf(event);
          s.open.set(body.id, { station, since: event.ts });
          s.calls.set(station, (s.calls.get(station) || 0) + 1);
          s.waiting = false;
          if (main) {
            const last = s.route[s.route.length - 1];
            if (!last || last.station !== station) s.route.push({ station, at: event.ts });
            else last.at = event.ts;
            if (s.route.length > ROUTE_LEGS + 1) s.route.shift();
          }
          if (live) pulse(station, false);
          break;
        }
        case "tool.finished": {
          const station = stationOf(event);
          const had = s.open.get(body.id);
          s.open.delete(body.id);
          if (!had) s.calls.set(station, (s.calls.get(station) || 0) + 1);
          if (body.outcome && body.outcome !== "ok") s.errors.set(station, (s.errors.get(station) || 0) + 1);
          if (live) pulse(station, true);
          break;
        }
        case "wait.started":
          s.waiting = true;
          if (live) pulse("you", false);
          break;
        case "subagent.started":
          s.subagents.add(body.id);
          break;
        case "subagent.finished":
          s.subagents.delete(body.id);
          break;
        case "compact":
          s.compactAt = event.ts;
          break;
        case "turn.finished":
        case "turn.failed":
          s.waiting = false;
          break;
        default:
          break;
      }
    }
    if (s.status && s.status !== "working" && s.status !== "blocked") s.open.clear();
    paint();
  }

  // The session's snapshot: its context window and status.
  function detail(d) {
    s.context = d && d.context ? d.context : null;
    s.status = (d && d.status) || "";
    if (s.status !== "working" && s.status !== "blocked") {
      // Calls left open by a session that stopped are not running.
      s.open.clear();
      s.subagents.clear();
    }
    if (s.status === "blocked") s.waiting = true;
    paint();
  }

  function reset() {
    s = fresh();
    pulses.replaceChildren();
    paint();
  }

  // --- painting ------------------------------------------------------------

  const clock = (ms, whole) => {
    const sec = Math.floor(ms / 1000);
    if (sec < 60) return `${sec}s`;
    if (sec < 3600) return `${Math.floor(sec / 60)}m${whole ? ` ${sec % 60}s` : ""}`;
    return `${Math.floor(sec / 3600)}h ${Math.floor((sec % 3600) / 60)}m`;
  };

  function paint(at) {
    const now = at || Date.now();
    const pct = s.context ? Math.round(s.context.pct) : null;
    ring.setAttribute("stroke-dasharray", `${pct == null ? 0 : Math.min(100, pct)} 100`);
    ring.setAttribute("class", `mp-ring${pct >= 90 ? " hot" : pct >= 70 ? " warm" : ""}${now - s.compactAt < 4000 ? " flash" : ""}`);
    pctText.textContent = pct == null ? "--" : `${pct}%`;
    statusText.textContent = s.status || "";
    root.classList.toggle("working", s.status === "working");

    const most = Math.max(1, ...s.calls.values());
    const running = new Map();
    for (const call of s.open.values()) {
      const r = running.get(call.station) || { n: 0, since: Infinity };
      r.n += 1;
      r.since = Math.min(r.since, call.since);
      running.set(call.station, r);
    }
    if (s.waiting) running.set("you", running.get("you") || { n: 1, since: now });
    if (s.subagents.size && !running.has("agents")) running.set("agents", { n: s.subagents.size, since: now });

    for (const [id, label] of STATIONS) {
      const p = parts.get(id);
      const calls = s.calls.get(id) || 0;
      const on = running.get(id);
      const errors = s.errors.get(id) || 0;
      p.spoke.style.strokeWidth = String(calls ? 1 + 4.5 * Math.sqrt(calls / most) : 1);
      p.spoke.classList.toggle("used", calls > 0);
      p.spoke.classList.toggle("on", !!on);
      p.g.classList.toggle("on", !!on);
      p.g.classList.toggle("idle", !calls && !on);
      p.count.textContent = calls ? String(calls) : "";
      p.many.textContent = on && on.n > 1 ? `x${on.n}` : "";
      const held = on && Number.isFinite(on.since) ? Math.max(0, now - on.since) : 0;
      p.inner.setAttribute("stroke-dasharray", `${on ? ((held % 60_000) / 60_000) * 100 : 0} 100`);
      p.outer.setAttribute("stroke-dasharray", `${on && held >= 60_000 ? ((held % 600_000) / 600_000) * 100 : 0} 100`);
      p.err.style.display = errors ? "" : "none";
      p.err.querySelector("text").textContent = String(errors);
      p.title.textContent = `${label}: ${calls} call${calls === 1 ? "" : "s"}${on ? `, ${on.n} running for ${clock(held, true)}` : ""}${errors ? `, ${errors} failed since the prompt` : ""}`;
    }

    // The route: legs between the last stations, fading with age.
    routeG.replaceChildren();
    for (let i = 1; i < s.route.length; i++) {
      const a = place.get(s.route[i - 1].station);
      const b = place.get(s.route[i].station);
      const age = now - s.route[i].at;
      const fade = age <= LEG_HOLD ? 1 : Math.max(0, 1 - (age - LEG_HOLD) / LEG_FADE);
      if (!a || !b || fade <= 0) continue;
      routeG.append(svg("line", { class: `mp-leg${i === s.route.length - 1 ? " newest" : ""}`, x1: a.x, y1: a.y, x2: b.x, y2: b.y, opacity: (0.25 + 0.75 * (i / (s.route.length - 1))) * fade }));
    }
  }

  return { node, feed, detail, reset, tick: () => paint() };
};

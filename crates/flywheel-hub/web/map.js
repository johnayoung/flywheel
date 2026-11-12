// flywheel hub: the live map of one agent session -- what it is doing,
// drawn instead of listed. A panel at the top of the Work drawer (beside
// the terminal, never instead of it), folded away with its toggle.
//
// The session sits in the centre, its context window as the ring around
// it (amber at 70%, red at 90%). Around it, the stations a call goes to by
// the kind of work it is (the hub tags each call: telemetry/station.rs):
// you (prompts, questions, permission waits), model (each step it
// reported, rate limits, compactions), plan, memory, tools (skills, tool
// search, anything else), agents, browser, network, waiting, shell,
// editing, reading. A call pulses out along its spoke when it starts and
// back when it ends; a station stays lit while a call there runs, with two
// clock hands growing round it (one a minute, one ten) and `xN` for calls
// at once; a failed or refused call leaves a red count until the next
// prompt. A spoke thickens the more its station is used; a thin line
// joins the last few stations the main agent went through, each leg
// fading after 40 seconds.
//
// Beyond the picture:
// * a station clicked opens what happened there: its files (reading,
//   editing, memory) or commands, and its last calls with how they ended;
// * replay: a turn plays back on the map in about twelve seconds, with a
//   scrubber to drag through it (work.js hands it the turn's events);
// * signals under the map, in words: the same call failing again and
//   again, a call running long, the context filling, a wait on you.
// Fed by work.js with the session's events, oldest first.
"use strict";

window.FwHubMap = function (opts) {
  const options = opts || {};
  const NS = "http://www.w3.org/2000/svg";
  const W = 520;
  const H = 272;
  const CX = W / 2;
  const CY = H / 2 + 2;
  const RX = 206;
  const RY = 100;
  // Clockwise from the top: the person, then what the agent keeps (plan,
  // memory), its tools and helpers, the outside world, waiting, then the
  // work on the code itself, and the model closing the circle.
  const STATIONS = [
    ["you", "you"],
    ["plan", "plan"],
    ["memory", "memory"],
    ["tools", "tools"],
    ["agents", "agents"],
    ["browser", "browser"],
    ["network", "network"],
    ["wait", "waiting"],
    ["shell", "shell"],
    ["edits", "editing"],
    ["files", "reading"],
    ["model", "model"],
  ];
  const LABEL = Object.fromEntries(STATIONS);
  // The hub's finer kinds the map shows together.
  const FOLD = { skills: "tools", other: "tools" };
  const FILE_STATIONS = new Set(["files", "edits", "memory"]);
  const ROUTE_LEGS = 6;
  const LEG_HOLD = 40_000;
  const LEG_FADE = 20_000;
  const PULSE_MS = 520;
  const RECENT = 40;
  const STREAK = 3; // failures in a row that read as stuck
  const LONG_CALL = 120_000;
  const REPLAY_MS = 12_000;
  const still = () => window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  const svg = (tag, attrs, ...kids) => {
    const node = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs || {})) node.setAttribute(k, String(v));
    for (const kid of kids) if (kid) node.append(kid);
    return node;
  };
  const h = (tag, attrs, ...kids) => {
    const node = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (k === "className") node.className = v;
      else if (k === "textContent") node.textContent = v;
      else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
      else node.setAttribute(k, String(v));
    }
    for (const kid of kids.flat()) if (kid != null && kid !== false) node.append(kid);
    return node;
  };

  const place = new Map(STATIONS.map(([id], i) => {
    const a = -Math.PI / 2 + (i / STATIONS.length) * Math.PI * 2;
    return [id, { x: CX + RX * Math.cos(a), y: CY + RY * Math.sin(a), a }];
  }));

  // --- state ---------------------------------------------------------------

  let s;
  function fresh() {
    return {
      calls: new Map(), // station -> calls seen
      ms: new Map(), // station -> time its finished calls took
      open: new Map(), // call id -> { station, since, what }
      errors: new Map(), // station -> failures since the prompt
      streak: new Map(), // station -> { n, what } failures in a row
      recent: new Map(), // station -> its last calls, newest last
      files: new Map(), // station -> Map(path -> touches)
      route: [], // { station, at } of the main agent
      subagents: new Set(),
      waiting: 0, // since when it waits on a person (0: not)
      compactAt: 0,
      limitedAt: 0,
      modelAt: 0,
      context: null,
      status: "",
      turnAt: 0,
      last: 0, // newest event's time
    };
  }
  s = fresh();
  let live = null; // the live session's latest snapshot, kept through a replay
  let selected = "";
  let replay = null; // { events, at, start, end, i, speed, playing, title, frame, lastFrame }

  const clockNow = () => (replay ? replay.at : Date.now());

  // --- the drawing ----------------------------------------------------------

  const root = svg("svg", { class: "mp-svg", viewBox: `0 0 ${W} ${H}`, role: "img", "aria-label": "Live map of the session" });
  const spokes = svg("g", { class: "mp-spokes" });
  const routeG = svg("g", { class: "mp-route" });
  const pulses = svg("g", { class: "mp-pulses" });
  const stationsG = svg("g", { class: "mp-stations" });
  const ring = svg("circle", { class: "mp-ring", cx: CX, cy: CY, r: 36, pathLength: 100, transform: `rotate(-90 ${CX} ${CY})` });
  const pctText = svg("text", { class: "mp-pct", x: CX, y: CY + 2 });
  const statusText = svg("text", { class: "mp-status", x: CX, y: CY + 15 });
  root.append(
    spokes,
    routeG,
    svg("circle", { class: "mp-ring-bed", cx: CX, cy: CY, r: 36 }),
    ring,
    svg("circle", { class: "mp-core", cx: CX, cy: CY, r: 29 }),
    pctText,
    statusText,
    pulses,
    stationsG,
  );

  const parts = new Map();
  for (const [id, label] of STATIONS) {
    const { x, y, a } = place.get(id);
    const spoke = svg("line", { class: "mp-spoke", x1: CX, y1: CY, x2: x, y2: y });
    spokes.append(spoke);
    const inner = svg("circle", { class: "mp-hand", cx: x, cy: y, r: 15, pathLength: 100, transform: `rotate(-90 ${x} ${y})` });
    const outer = svg("circle", { class: "mp-hand outer", cx: x, cy: y, r: 18.5, pathLength: 100, transform: `rotate(-90 ${x} ${y})` });
    const dot = svg("circle", { class: `mp-node st-${id}`, cx: x, cy: y, r: 11.5 });
    const count = svg("text", { class: "mp-count", x, y: y + 3.2 });
    // Labels sit outside the ellipse, away from the centre.
    const lx = x + Math.cos(a) * 26;
    const ly = y + Math.sin(a) * 22 + 3;
    const anchor = Math.abs(Math.cos(a)) < 0.3 ? "middle" : Math.cos(a) > 0 ? "start" : "end";
    const name = svg("text", { class: "mp-label", x: lx, y: ly, "text-anchor": anchor });
    name.textContent = label;
    const many = svg("text", { class: "mp-many", x: x + 15, y: y - 13 });
    const err = svg("g", { class: "mp-err", transform: `translate(${x - 13} ${y - 12})` },
      svg("circle", { r: 6 }), svg("text", { y: 2.8 }));
    const hit = svg("circle", { class: "mp-hit", cx: x, cy: y, r: 20 });
    const title = svg("title", {});
    const g = svg("g", { class: "mp-st", "data-station": id, tabindex: 0, role: "button" }, hit, inner, outer, dot, count, name, many, err, title);
    g.addEventListener("click", () => select(id));
    g.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        select(id);
      }
    });
    stationsG.append(g);
    parts.set(id, { g, spoke, inner, outer, count, many, err, title });
  }

  const signals = h("div", { className: "mp-signals", "aria-live": "polite" });
  const playBtn = h("button", { type: "button", className: "mp-btn", onclick: () => togglePlay() });
  const liveBtn = h("button", { type: "button", className: "mp-btn", textContent: "live", title: "Back to the session as it is now", onclick: () => stopReplay() });
  const lastBtn = h("button", { type: "button", className: "mp-btn", textContent: "replay last turn", title: "Play the newest turn back on the map", onclick: () => options.onReplayLast && options.onReplayLast() });
  const track = h("div", { className: "mp-track", title: "Drag to move through the turn" }, h("i", { className: "mp-fill" }));
  const replayTitle = h("span", { className: "mp-rp-title" });
  const replayTime = h("span", { className: "mp-rp-time" });
  const replayBar = h("div", { className: "mp-replay", hidden: "" },
    playBtn, h("div", { className: "mp-rp-mid" }, h("div", { className: "mp-rp-head" }, replayTitle, replayTime), track), liveBtn);
  const foot = h("div", { className: "mp-foot" }, signals, lastBtn);
  const panel = h("div", { className: "mp-panel", hidden: "" });

  const node = h("div", { className: "mp" });
  const stage = h("div", { className: "mp-stage" });
  stage.append(root);
  node.append(stage, replayBar, foot, panel);

  // Dragging the scrubber seeks.
  let dragging = false;
  const seekAt = (event) => {
    if (!replay) return;
    const box = track.getBoundingClientRect();
    const frac = Math.min(1, Math.max(0, (event.clientX - box.left) / box.width));
    seek(replay.start + frac * (replay.end - replay.start));
  };
  track.addEventListener("pointerdown", (event) => {
    dragging = true;
    track.setPointerCapture(event.pointerId);
    seekAt(event);
  });
  track.addEventListener("pointermove", (event) => dragging && seekAt(event));
  track.addEventListener("pointerup", () => {
    dragging = false;
  });

  // --- feeding -------------------------------------------------------------

  const stationOf = (event) => {
    const raw = event.station || "other";
    return FOLD[raw] || raw;
  };
  const bump = (map, key, by) => map.set(key, (map.get(key) || 0) + (by == null ? 1 : by));

  function pulse(station, back) {
    if (still() || !node.isConnected) return;
    const to = place.get(station);
    if (!to) return;
    const from = back ? to : { x: CX, y: CY };
    const end = back ? { x: CX, y: CY } : to;
    const dot = svg("circle", { class: `mp-pulse${back ? " back" : ""}`, r: 3.2, cx: from.x, cy: from.y });
    pulses.append(dot);
    const anim = dot.animate(
      [{ transform: "translate(0,0)" }, { transform: `translate(${end.x - from.x}px,${end.y - from.y}px)` }],
      { duration: replay ? Math.min(PULSE_MS, 360) : PULSE_MS, easing: "ease-out" },
    );
    anim.onfinish = () => dot.remove();
  }

  function remember(station, entry) {
    if (!s.recent.has(station)) s.recent.set(station, []);
    const list = s.recent.get(station);
    list.push(entry);
    if (list.length > RECENT) list.shift();
  }

  // One event into the state; `animate`: pulse it.
  function apply(event, animate) {
    const body = event.body || {};
    const main = !event.agent;
    s.last = Math.max(s.last, event.ts);
    switch (event.kind) {
      case "turn.started":
        s.errors.clear();
        s.streak.clear();
        s.waiting = 0;
        s.turnAt = event.ts;
        // A prompt: from you to the session.
        bump(s.calls, "you");
        remember("you", { id: `p${event.seq}`, name: "prompt", what: body.title || "", at: event.ts, ms: 0, outcome: "ok" });
        if (animate) pulse("you", true);
        break;
      case "tool.started": {
        const station = stationOf(event);
        const what = body.summary || "";
        s.open.set(body.id, { station, since: event.ts, what, name: body.name });
        bump(s.calls, station);
        s.waiting = 0;
        remember(station, { id: body.id, name: body.name, what, at: event.ts, ms: null, outcome: "" });
        if (FILE_STATIONS.has(station) && what && !what.includes(" ")) {
          if (!s.files.has(station)) s.files.set(station, new Map());
          bump(s.files.get(station), what);
        }
        if (main) {
          const last = s.route[s.route.length - 1];
          if (!last || last.station !== station) s.route.push({ station, at: event.ts });
          else last.at = event.ts;
          if (s.route.length > ROUTE_LEGS + 1) s.route.shift();
        }
        if (animate) pulse(station, false);
        break;
      }
      case "tool.finished": {
        const station = stationOf(event);
        const had = s.open.get(body.id);
        s.open.delete(body.id);
        const what = body.summary || (had && had.what) || "";
        const ms = Number.isFinite(body.duration_ms) ? body.duration_ms : had ? event.ts - had.since : 0;
        const outcome = body.outcome || "ok";
        if (!had) {
          bump(s.calls, station);
          remember(station, { id: body.id, name: body.name, what, at: event.ts - ms, ms, outcome });
        } else {
          const entry = (s.recent.get(station) || []).find((e) => e.id === body.id);
          if (entry) {
            entry.ms = ms;
            entry.outcome = outcome;
            if (body.error) entry.error = body.error;
          }
        }
        bump(s.ms, station, ms);
        if (outcome !== "ok") {
          bump(s.errors, station);
          const streak = s.streak.get(station) || { n: 0, what: "" };
          s.streak.set(station, { n: streak.n + 1, what: what || streak.what });
        } else {
          s.streak.delete(station);
        }
        if (animate) pulse(station, true);
        break;
      }
      case "wait.started":
        s.waiting = event.ts;
        bump(s.calls, "you");
        remember("you", { id: `w${event.seq}`, name: "waiting", what: body.reason === "permission" ? `permission${body.tool ? `: ${body.tool}` : ""}` : body.reason || "a person", at: event.ts, ms: null, outcome: "" });
        if (animate) pulse("you", false);
        break;
      case "ask":
        bump(s.calls, "you");
        remember("you", { id: `a${event.seq}`, name: "asked", what: body.text || "", at: event.ts, ms: 0, outcome: "ok" });
        if (animate) pulse("you", false);
        break;
      case "subagent.started":
        s.subagents.add(body.id);
        break;
      case "subagent.finished":
        s.subagents.delete(body.id);
        break;
      case "usage":
      case "context.usage":
        if (event.kind === "context.usage" && body.window) {
          s.context = { pct: (body.used / body.window) * 100, used: body.used, window: body.window };
        }
        // One model step reports both: count it once.
        if (event.ts - s.modelAt > 1500) {
          bump(s.calls, "model");
          if (animate) pulse("model", true);
        }
        s.modelAt = event.ts;
        break;
      case "rate.limited":
        s.limitedAt = event.ts;
        bump(s.errors, "model");
        remember("model", { id: `r${event.seq}`, name: "rate limit", what: body.detail || "refused", at: event.ts, ms: 0, outcome: "error" });
        break;
      case "compact":
        s.compactAt = event.ts;
        remember("model", { id: `c${event.seq}`, name: "compact", what: body.trigger || body.phase || "", at: event.ts, ms: 0, outcome: "ok" });
        break;
      case "turn.failed":
        bump(s.errors, "model");
        remember("model", { id: `f${event.seq}`, name: "turn failed", what: body.error || "", at: event.ts, ms: 0, outcome: "error" });
        s.waiting = 0;
        break;
      case "turn.finished":
        s.waiting = 0;
        if (animate) pulse("you", false);
        break;
      default:
        break;
    }
  }

  // Live events, oldest first; `animate`: they just happened.
  function feed(events, animate) {
    if (replay) return; // played again from work.js on the way back
    for (const event of events) apply(event, animate);
    settle();
    paint();
  }

  // A session that stopped runs nothing.
  function settle() {
    if (s.status && s.status !== "working" && s.status !== "blocked") {
      s.open.clear();
      s.subagents.clear();
    }
    if (s.status === "blocked" && !s.waiting) s.waiting = s.last || Date.now();
  }

  // The session's snapshot: its context window and status.
  function detail(d) {
    live = d || null;
    if (replay) return;
    if (d && d.context) s.context = d.context;
    s.status = (d && d.status) || "";
    settle();
    paint();
  }

  function reset() {
    stopReplay(true);
    s = fresh();
    live = null;
    selected = "";
    pulses.replaceChildren();
    paint();
  }

  // --- selecting a station --------------------------------------------------

  function select(id) {
    selected = selected === id ? "" : id;
    paint();
  }

  const base = (path) => path.split("/").filter(Boolean).pop() || path;
  const took = (ms) => {
    if (ms == null) return "";
    if (ms < 1000) return `${Math.round(ms)}ms`;
    const sec = Math.floor(ms / 1000);
    if (sec < 60) return `${sec}s`;
    if (sec < 3600) return `${Math.floor(sec / 60)}m ${sec % 60}s`;
    return `${Math.floor(sec / 3600)}h ${Math.floor((sec % 3600) / 60)}m`;
  };

  function paintPanel(now) {
    if (!selected) {
      panel.hidden = true;
      for (const p of parts.values()) p.g.classList.remove("sel");
      return;
    }
    for (const [id, p] of parts) p.g.classList.toggle("sel", id === selected);
    const calls = s.calls.get(selected) || 0;
    const errors = s.errors.get(selected) || 0;
    const recent = (s.recent.get(selected) || []).slice(-8).reverse();
    const files = s.files.get(selected);
    const kids = [
      h("div", { className: "mp-pn-head" },
        h("b", { className: `mp-pn-name st-${selected}` }, h("i", { className: "mp-pn-dot" }), LABEL[selected] || selected),
        h("span", { className: "mp-pn-sum", textContent: `${calls} ${selected === "model" ? "step" : "call"}${calls === 1 ? "" : "s"}${s.ms.get(selected) ? ` · ${took(s.ms.get(selected))}` : ""}${errors ? ` · ${errors} failed this turn` : ""}` }),
        h("button", { type: "button", className: "mp-btn mp-pn-x", textContent: "close", onclick: () => select(selected) })),
    ];
    if (files && files.size) {
      const top = [...files.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8);
      kids.push(h("div", { className: "mp-pn-files" }, top.map(([path, n]) => h("span", { className: "mp-pn-file", title: path }, base(path), n > 1 ? h("em", { textContent: ` x${n}` }) : null))));
    }
    if (recent.length) {
      kids.push(h("ol", { className: "mp-pn-calls" }, recent.map((c) => {
        const running = c.ms == null && s.open.has(c.id);
        const state = running ? "running" : c.outcome && c.outcome !== "ok" ? "bad" : "";
        return h("li", { className: state },
          h("span", { className: "mp-pn-at", textContent: new Date(c.at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" }) }),
          h("b", { textContent: c.name || "" }),
          h("span", { className: "mp-pn-what", textContent: c.what || "", title: c.error ? `${c.what}\n${c.error}` : c.what || "" }),
          h("span", { className: "mp-pn-ms", textContent: running ? `${took(now - c.at)}...` : c.outcome && c.outcome !== "ok" ? c.outcome : took(c.ms) }));
      })));
    } else {
      kids.push(h("p", { className: "mp-pn-none", textContent: "Nothing here in the events read." }));
    }
    panel.replaceChildren(...kids);
    panel.hidden = false;
  }

  // --- signals --------------------------------------------------------------

  function paintSignals(now) {
    const out = [];
    for (const [station, st] of s.streak) {
      if (st.n >= STREAK) out.push(["bad", `${LABEL[station] || station} failed ${st.n} times in a row${st.what ? `: ${st.what}` : ""}`, station]);
    }
    if (s.waiting) out.push(["bad", `waiting on you ${took(now - s.waiting)}`, "you"]);
    for (const call of s.open.values()) {
      const held = now - call.since;
      if (held >= LONG_CALL) out.push(["warn", `${LABEL[call.station] || call.station} running ${took(held)}${call.what ? `: ${call.what}` : ""}`, call.station]);
    }
    const pct = s.context ? s.context.pct : null;
    if (pct >= 80) out.push([pct >= 90 ? "bad" : "warn", `context ${Math.round(pct)}% full`, "model"]);
    if (s.limitedAt && now - s.limitedAt < 600_000) out.push(["bad", "rate limit refused a request", "model"]);
    if (!out.length) {
      const doing = [...s.open.values()].sort((a, b) => b.since - a.since)[0];
      if (doing) out.push(["", `now: ${LABEL[doing.station] || doing.station}${doing.what ? ` -- ${doing.what}` : ""}`, doing.station]);
      else if (s.status && !replay) out.push(["quiet", s.status === "working" ? "thinking" : `${s.status}`, ""]);
    }
    signals.replaceChildren(...out.slice(0, 3).map(([tone, text, station]) => h("button", {
      type: "button",
      className: `mp-sig ${tone}`,
      title: station ? "Show what happened there" : text,
      onclick: () => station && select(station),
    }, text)));
  }

  // --- painting ------------------------------------------------------------

  function paint() {
    const now = clockNow();
    const pct = s.context ? Math.round(s.context.pct) : null;
    ring.setAttribute("stroke-dasharray", `${pct == null ? 0 : Math.min(100, pct)} 100`);
    ring.setAttribute("class", `mp-ring${pct >= 90 ? " hot" : pct >= 70 ? " warm" : ""}${now - s.compactAt < 4000 ? " flash" : ""}`);
    pctText.textContent = pct == null ? "--" : `${pct}%`;
    statusText.textContent = replay ? "replay" : s.status || "";
    root.classList.toggle("working", !replay && s.status === "working");
    root.classList.toggle("replaying", !!replay);

    const most = Math.max(1, ...s.calls.values());
    const running = new Map();
    for (const call of s.open.values()) {
      const r = running.get(call.station) || { n: 0, since: Infinity };
      r.n += 1;
      r.since = Math.min(r.since, call.since);
      running.set(call.station, r);
    }
    if (s.waiting) running.set("you", { n: 1, since: s.waiting });
    if (s.subagents.size && !running.has("agents")) running.set("agents", { n: s.subagents.size, since: NaN });
    if (now - s.modelAt < 1200 && !running.has("model")) running.set("model", { n: 1, since: NaN, blink: true });

    for (const [id, label] of STATIONS) {
      const p = parts.get(id);
      const calls = s.calls.get(id) || 0;
      const on = running.get(id);
      const errors = s.errors.get(id) || 0;
      const streak = (s.streak.get(id) || { n: 0 }).n >= STREAK;
      p.spoke.style.strokeWidth = String(calls ? 1 + 4.5 * Math.sqrt(calls / most) : 1);
      p.spoke.classList.toggle("used", calls > 0);
      p.spoke.classList.toggle("on", !!on);
      p.g.classList.toggle("on", !!on);
      p.g.classList.toggle("stuck", streak);
      p.g.classList.toggle("idle", !calls && !on);
      p.count.textContent = calls ? (calls > 999 ? `${Math.round(calls / 100) / 10}k` : String(calls)) : "";
      p.many.textContent = on && on.n > 1 ? `x${on.n}` : "";
      const held = on && Number.isFinite(on.since) ? Math.max(0, now - on.since) : 0;
      p.inner.setAttribute("stroke-dasharray", `${held ? ((held % 60_000) / 60_000) * 100 : 0} 100`);
      p.outer.setAttribute("stroke-dasharray", `${held >= 60_000 ? ((held % 600_000) / 600_000) * 100 : 0} 100`);
      p.err.style.display = errors ? "" : "none";
      p.err.querySelector("text").textContent = String(errors);
      p.title.textContent = `${label}: ${calls} ${id === "model" ? "step" : "call"}${calls === 1 ? "" : "s"}${on && held ? `, running ${took(held)}` : ""}${errors ? `, ${errors} failed this turn` : ""} (click for detail)`;
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

    paintSignals(now);
    paintPanel(now);
    paintReplay();
  }

  // --- replay ---------------------------------------------------------------

  // Play `events` (one turn's, oldest first) back on the map.
  function startReplay(events, title) {
    const list = (events || []).filter((e) => Number.isFinite(e.ts)).sort((a, b) => a.ts - b.ts || a.seq - b.seq);
    if (!list.length) return false;
    stopReplay(true);
    const start = list[0].ts;
    const end = Math.max(start + 1, list[list.length - 1].ts);
    replay = { events: list, start, end, at: start, i: 0, speed: Math.max(1, (end - start) / REPLAY_MS), playing: true, title: title || "turn", frame: 0, lastFrame: 0 };
    s = fresh();
    s.status = "replay";
    pulses.replaceChildren();
    node.classList.add("replaying");
    replayBar.hidden = false;
    lastBtn.hidden = true;
    step(true);
    loop();
    return true;
  }

  // Apply the events up to the replay's clock.
  function step(animate) {
    while (replay.i < replay.events.length && replay.events[replay.i].ts <= replay.at) {
      apply(replay.events[replay.i], animate);
      replay.i += 1;
    }
    paint();
  }

  function loop() {
    if (!replay) return;
    cancelAnimationFrame(replay.frame);
    replay.lastFrame = performance.now();
    const tickFrame = (t) => {
      if (!replay || !replay.playing) return;
      const dt = Math.min(100, t - replay.lastFrame);
      replay.lastFrame = t;
      // Long quiet stretches pass quickly: no event for a while, jump on.
      const next = replay.events[replay.i];
      replay.at += dt * replay.speed;
      if (next && next.ts - replay.at > replay.speed * 1500) replay.at = next.ts - replay.speed * 300;
      if (replay.at >= replay.end) {
        replay.at = replay.end;
        step(true);
        replay.playing = false;
        paintReplay();
        return;
      }
      step(true);
      replay.frame = requestAnimationFrame(tickFrame);
    };
    replay.frame = requestAnimationFrame(tickFrame);
  }

  function seek(at) {
    if (!replay) return;
    s = fresh();
    s.status = "replay";
    pulses.replaceChildren();
    replay.i = 0;
    replay.at = at;
    step(false);
  }

  function togglePlay() {
    if (!replay) return;
    if (replay.at >= replay.end) seek(replay.start);
    replay.playing = !replay.playing;
    if (replay.playing) loop();
    paintReplay();
  }

  // Back to the live session (`quiet`: without asking work.js to refeed).
  function stopReplay(quiet) {
    if (!replay) return;
    cancelAnimationFrame(replay.frame);
    replay = null;
    node.classList.remove("replaying");
    replayBar.hidden = true;
    lastBtn.hidden = false;
    pulses.replaceChildren();
    if (!quiet) {
      s = fresh();
      if (options.onLive) options.onLive();
      detail(live);
    }
  }

  function paintReplay() {
    if (!replay) return;
    const frac = (replay.at - replay.start) / (replay.end - replay.start);
    track.firstChild.style.width = `${Math.min(100, frac * 100)}%`;
    replayTitle.textContent = replay.title;
    replayTime.textContent = `${took(replay.at - replay.start)} of ${took(replay.end - replay.start)}`;
    playBtn.textContent = replay.playing ? "pause" : replay.at >= replay.end ? "again" : "play";
  }

  return {
    node,
    feed,
    detail,
    reset,
    replay: startReplay,
    replaying: () => !!replay,
    tick: () => {
      if (!replay) paint();
    },
  };
};

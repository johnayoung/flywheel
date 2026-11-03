// flywheel hub: the "why?" beside the attached agent's status pill. Opens
// a popover with GET /api/sessions/{id}/explain: what set the status (a
// hook, a detection manifest rule, a heuristic), the rule the live screen
// matches now with its evidence lines, every rule tried, and the note
// that idle is not authorization. app.js creates it with its helpers.
"use strict";

window.FwHubExplain = function (ctx) {
  const { $, el, api } = ctx;

  const button = $("#crumb-why");
  const pop = el("div", { className: "why-pop", id: "why-pop", role: "dialog", "aria-label": "Why this status" });
  pop.hidden = true;
  document.body.append(pop);

  const view = {
    session: null, // the attached session's id
    stamp: "", // status + since the open popover was drawn for
    seq: 0, // drops answers to superseded requests
  };

  const sourceText = (source, label) => {
    if (!source) return "nothing yet";
    switch (source.kind) {
      case "hook":
        return `hook: ${source.agent} ${source.event}`;
      case "manifest":
        return label;
      case "heuristic":
        return `heuristic: ${source.reason}`;
      case "hub":
        return `hub: ${source.reason}`;
      default:
        return "nothing yet (no report, no screen rule)";
    }
  };

  function row(name, ...value) {
    return el("div", { className: "why-row" }, el("span", { className: "why-key" }, name), el("span", { className: "why-val" }, ...value));
  }

  function draw(data) {
    const status = data.status ? data.status.status : "none";
    const manifest = data.manifest || {};
    const origin = manifest.origin === "override" ? `override ${manifest.path}` : "bundled";
    const screen = data.screen;
    let screenLine;
    if (!screen) screenLine = "no terminal on this hub";
    else if (screen.state) screenLine = `${screen.state} by ${manifest.file}#${screen.rule}${screen.detail ? ` -- ${screen.detail}` : ""}`;
    else screenLine = screen.fallback || "no rule matched";
    const evidence = screen && screen.evidence && screen.evidence.length
      ? el("pre", { className: "why-evidence" }, screen.evidence.map((e) => `${String(e.row).padStart(3)}  ${e.text.trim()}\n`).join(""))
      : null;
    const rules = (manifest.rules || []).map((r) =>
      el("li", { className: r.matched ? "hit" : "" },
        el("b", {}, r.id), ` ${r.state} `,
        el("span", { className: "why-reason" }, r.matched ? "matched" : r.reason || "")),
    );
    const problems = (data.problems || []).map((p) => el("li", {}, `${p.path}: ${p.error}`));
    pop.replaceChildren(...[
      el("div", { className: "why-head" },
        el("span", { className: `agent-pill ${status}` }, status),
        el("span", { className: "why-title" }, "why?"),
        el("button", { type: "button", className: "why-close", "aria-label": "Close", onclick: close }, "×")),
      data.status ? row("set by", sourceText(data.source, data.source_label)) : row("status", "shell sessions have no agent status"),
      data.status && data.status.detail ? row("waits on", data.status.detail) : null,
      row("screen now", screenLine),
      evidence,
      row("manifest", `${manifest.file} (${origin}, version ${manifest.version})`),
      rules.length ? el("details", { className: "why-rules" }, el("summary", {}, `${rules.length} rules, in order`), el("ol", {}, rules)) : null,
      problems.length ? el("div", { className: "why-problems" }, el("b", {}, "overrides not in force"), el("ul", {}, problems)) : null,
      el("p", { className: "why-note" }, data.note || ""),
    ].filter(Boolean));
  }

  function place() {
    const box = button.getBoundingClientRect();
    const width = Math.min(440, window.innerWidth - 16);
    pop.style.width = `${width}px`;
    pop.style.left = `${Math.max(8, Math.min(box.left, window.innerWidth - width - 8))}px`;
    pop.style.top = `${box.bottom + 6}px`;
  }

  async function load() {
    const id = view.session;
    const seq = ++view.seq;
    try {
      const data = await api("GET", `/api/sessions/${encodeURIComponent(id)}/explain`);
      if (seq !== view.seq || pop.hidden) return;
      draw(data);
      place();
    } catch (e) {
      if (seq !== view.seq || pop.hidden) return;
      pop.replaceChildren(el("p", { className: "why-note" }, `Could not explain: ${e.message}`));
    }
  }

  function open() {
    if (!view.session) return;
    pop.hidden = false;
    button.setAttribute("aria-expanded", "true");
    pop.replaceChildren(el("p", { className: "why-note" }, "Reading the screen…"));
    place();
    load();
  }

  function close() {
    if (pop.hidden) return;
    pop.hidden = true;
    view.seq++;
    button.setAttribute("aria-expanded", "false");
  }

  button.addEventListener("click", (event) => {
    event.stopPropagation();
    if (pop.hidden) open();
    else close();
  });
  document.addEventListener("pointerdown", (event) => {
    if (!pop.hidden && !pop.contains(event.target) && event.target !== button) close();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !pop.hidden) {
      close();
      button.focus();
    }
  });
  window.addEventListener("resize", () => {
    if (!pop.hidden) place();
  });

  // Called on every render: show the button for an agent session; an
  // open popover follows the attached session and its status changes.
  function render(session, agent) {
    const id = session && agent ? session.id : null;
    button.hidden = !id;
    if (id !== view.session) {
      view.session = id;
      close();
    }
    if (!agent) return;
    button.title = `Why ${agent.status}? (what set it, what the screen shows)`;
    const stamp = `${agent.status} ${agent.since}`;
    if (stamp !== view.stamp) {
      view.stamp = stamp;
      if (!pop.hidden) load();
    }
  }

  return { render, close };
};

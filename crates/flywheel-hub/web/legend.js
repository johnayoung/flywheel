// flywheel hub: the state legend. Every state an agent session shows, in
// the order one outranks the next (the hub works the state out once:
// status.rs shown_state), with what it means, what enters and leaves it,
// and the group its row sits in. Drawn inside the "why?" popover (the
// attached session's state marked) and in its own popover from the
// sidebar's Moving heading. app-base.js creates it with its helpers.
"use strict";

window.FwHubLegend = function (ctx) {
  const { el } = ctx;

  // [key, dot, word, means, enters on, leaves on, group]
  const STATES = [
    ["blocked", "blocked", "blocked", "a permission or question dialog is open",
      "PermissionRequest; a permission or input notice; a known dialog on screen",
      "the call runs, the turn ends, or a prompt", "Needs you"],
    ["asking", "asking", "asking", "its turn ended asking you",
      "ask_operator, or a closing message ending on a question",
      "a prompt or a handoff taken up; a closing question once seen", "Needs you"],
    ["compacting", "compacting", "compacting", "its context is being compacted",
      "PreCompact", "PostCompact, a prompt, or the turn's end", "Moving"],
    ["working", "working", "working", "a turn runs: the call and its age, or thinking",
      "UserPromptSubmit, a call (a prompt mid-turn keeps the turn)",
      "Stop, StopFailure, an interrupt, Esc seen on screen", "Moving"],
    ["failed", "bad", "failed", "its turn ended on an error",
      "StopFailure", "you view it, or a prompt", "Needs you"],
    ["background", "background", "background", "the turn is over; subagents it started still run",
      "Stop while a subagent runs", "the last subagent ends (then done)", "Moving"],
    ["quiet", "quiet", "quiet Nm", "moving, but no event for 5 min or more (an age, not a guess)",
      "5 min without an event", "any event", "Moving"],
    ["done", "agent-done", "done", "its turn finished and you have not looked",
      "Stop, nothing left running", "you view it", "Needs you"],
    ["idle", "idle", "resting", "finished and seen, or no prompt yet",
      "a done turn viewed; a new conversation", "a prompt", "Resting"],
    ["handed", "handed", "handed off", "its work went on in another session",
      "a handoff taken up, or a replacement", "", "Resting"],
    ["ended", "stopped", "ended", "the agent ended, or has said nothing yet",
      "SessionEnd, or its process exits", "a relaunch", "Resting"],
  ];

  // The legend as a table; `current` (a key above) marks that row.
  function table(current) {
    const head = el("tr", {}, ["state", "means", "enters on", "leaves on", "group"].map((h) => el("th", { textContent: h })));
    const rows = STATES.map(([key, dot, word, means, enters, leaves, group]) =>
      el(
        "tr",
        { className: key === current ? "here" : "", "aria-current": key === current ? "true" : undefined },
        el("td", { className: "lg-state" }, el("span", { className: `dot ${dot}`, "aria-hidden": "true" }), el("b", { textContent: word })),
        el("td", { textContent: means }),
        el("td", { textContent: enters }),
        el("td", { textContent: leaves || "—" }),
        el("td", { textContent: group }),
      ));
    return el("table", { className: "legend" }, el("thead", {}, head), el("tbody", {}, rows));
  }

  // The legend key of what a session shows now (its status look:
  // app-base.js statusLook), else "".
  function keyOf(look) {
    if (!look || !look.state) return "";
    if (look.quiet) return "quiet";
    if (look.dot === "handed") return "handed";
    return look.state === "unknown" ? "ended" : look.state;
  }

  // Its own popover, under `trigger` (the sidebar's entry).
  const pop = el("div", { className: "why-pop legend-pop", id: "legend-pop", role: "dialog", "aria-label": "What each state means" });
  pop.hidden = true;
  document.body.append(pop);
  let anchor = null;

  function open(trigger, current) {
    anchor = trigger;
    pop.replaceChildren(
      el("div", { className: "why-head" },
        el("span", { className: "why-title", textContent: "states, most urgent first" }),
        el("button", { type: "button", className: "why-close", "aria-label": "Close", onclick: close }, "×")),
      table(current),
    );
    pop.hidden = false;
    trigger.setAttribute("aria-expanded", "true");
    window.FwHubFmt.placeUnder(pop, trigger, 720);
  }

  function close() {
    if (pop.hidden) return;
    pop.hidden = true;
    if (anchor) anchor.setAttribute("aria-expanded", "false");
    anchor = null;
  }

  function toggle(trigger, current) {
    if (!pop.hidden && anchor === trigger) close();
    else open(trigger, current);
  }

  document.addEventListener("pointerdown", (event) => {
    if (!pop.hidden && !pop.contains(event.target) && !(anchor && anchor.contains(event.target))) close();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !pop.hidden) close();
  });
  window.addEventListener("resize", () => {
    if (!pop.hidden && anchor && anchor.isConnected) window.FwHubFmt.placeUnder(pop, anchor, 720);
  });

  return { table, keyOf, toggle, close };
};

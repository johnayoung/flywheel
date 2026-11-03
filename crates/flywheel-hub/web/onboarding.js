// flywheel hub: the first run. With no workspace yet, the start screen
// offers the two ways in -- import a VS Code workspace found under the
// import root (GET /api/code-workspaces, POST /api/workspaces/import), or
// make one from a folder (POST /api/workspaces) -- and a 30-second tour of
// the key shortcuts (each shown with its binding in force). The tour is
// offered while the `keyboard.tour` preference is on; finishing or
// dismissing it turns that off (kept by the hub, so every device knows).
// app-dialogs.js creates it with its helpers.
"use strict";

window.FwHubOnboarding = function (ctx) {
  const { $, el, icon, api, refresh, rememberWorkspace, basename, prefs, openWorkspaceDialog, toast } = ctx;

  const TOUR_PREF = "keyboard.tour";
  // Each step stays this long unless paused (pointer over the card, or a
  // key or click in it); six steps: the 30 seconds.
  const STEP_MS = 5000;
  // Files offered on the start screen (the dialog lists them all).
  const SHOWN_FILES = 4;

  const ui = {
    scan: null, // the last GET /api/code-workspaces answer
    scanning: false,
    scanError: "",
    busy: false,
    folderError: "",
    importError: "",
  };

  const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
  // A touch-only device has no keys to learn (palette "Shortcut tour"
  // still plays it).
  const touchOnly = window.matchMedia("(hover: none) and (pointer: coarse)");
  const tourOffered = () => prefs.get(TOUR_PREF, true) !== false;

  // --- the start screen ---------------------------------------------------

  async function scan() {
    if (ui.scanning) return;
    ui.scanning = true;
    ui.scanError = "";
    try {
      ui.scan = await api("GET", "/api/code-workspaces");
    } catch (error) {
      ui.scanError = error.message;
    } finally {
      ui.scanning = false;
      ctx.rerender();
    }
  }

  async function importFile(path) {
    if (ui.busy) return;
    ui.busy = true;
    ui.importError = "";
    ctx.rerender();
    try {
      const done = await api("POST", "/api/workspaces/import", { path, update: false });
      rememberWorkspace(done.workspace.id);
      const skipped = done.skipped.length ? `; ${plural(done.skipped.length, "folder")} skipped (not on this machine)` : "";
      toast(`Imported ${done.workspace.name}: ${plural(done.workspace.folders.length, "folder")}${skipped}.`, { timeout: 5000 });
      await refresh();
    } catch (error) {
      ui.importError = error.message;
    } finally {
      ui.busy = false;
      ctx.rerender();
    }
  }

  // Every listed file not imported yet, one after another (each its own
  // workspace); stops at the first failure, which stays shown.
  async function importAll(paths) {
    if (ui.busy || !paths.length) return;
    ui.busy = true;
    ui.importError = "";
    ctx.rerender();
    let done = 0;
    try {
      for (const path of paths) {
        const result = await api("POST", "/api/workspaces/import", { path, update: false });
        if (!done) rememberWorkspace(result.workspace.id);
        done += 1;
      }
    } catch (error) {
      ui.importError = error.message;
    } finally {
      if (done) toast(`Imported ${plural(done, "workspace")}.`, { timeout: 5000 });
      await refresh().catch(() => {});
      ui.busy = false;
      ctx.rerender();
    }
  }

  async function createFromFolder(raw) {
    const path = raw.trim();
    if (!path || ui.busy) return;
    ui.busy = true;
    ui.folderError = "";
    ctx.rerender();
    try {
      const checked = await api("GET", `/api/folders/check?path=${encodeURIComponent(path)}`);
      if (!checked.ok) throw new Error(checked.error);
      const created = await api("POST", "/api/workspaces", {
        name: basename(checked.path),
        folders: [checked.path],
      });
      rememberWorkspace(created.id);
      toast(`Workspace ${created.name} created${checked.git ? "" : " (a plain folder: no git badges or worktrees)"}.`, { timeout: 4000 });
      await refresh();
    } catch (error) {
      ui.folderError = error.message;
    } finally {
      ui.busy = false;
      ctx.rerender();
    }
  }

  function importCard() {
    const found = ui.scan;
    const pending = found ? found.files.filter((f) => !f.error && !f.workspace_id).map((f) => f.path) : [];
    let list;
    if (ui.scanError) {
      list = el("p", { className: "first-run-note bad", textContent: ui.scanError });
    } else if (!found) {
      list = el("p", { className: "first-run-note", textContent: "Looking for *.code-workspace files..." });
    } else if (!found.files.length) {
      list = el("p", {
        className: "first-run-note",
        textContent: `None within ${found.depth} levels of ${found.root}.`,
      });
    } else {
      const files = found.files.filter((f) => !f.error);
      list = el(
        "ul",
        { className: "first-run-files" },
        files.slice(0, SHOWN_FILES).map((file) =>
          el(
            "li",
            {},
            el(
              "span",
              { className: "first-run-file" },
              el("b", { textContent: file.name || basename(file.path) }),
              el("small", {
                textContent: `${plural(file.folders, "folder")}${file.skipped ? ` · ${file.skipped} not local` : ""}`,
                title: file.path,
              }),
            ),
            file.workspace_id
              ? el("span", { className: "first-run-done", textContent: "imported" })
              : el("button", {
                type: "button",
                className: "slab",
                disabled: ui.busy,
                textContent: "Import",
                title: `Create workspace ${file.name} from ${file.path}`,
                onclick: () => importFile(file.path),
              }),
          )),
        files.length > SHOWN_FILES
          ? el("li", { className: "first-run-more", textContent: `and ${files.length - SHOWN_FILES} more (Browse...)` })
          : null,
      );
    }
    return el(
      "section",
      { className: "first-run-card", "aria-labelledby": "first-run-import" },
      el("h2", { id: "first-run-import" }, icon("import"), "Import a VS Code workspace"),
      el("p", {
        textContent: found
          ? `Multi-root workspaces under ${found.root}: their folders and names become a workspace.`
          : "Multi-root workspaces: their folders and names become a workspace.",
      }),
      list,
      ui.importError ? el("p", { className: "first-run-note bad", role: "alert", textContent: ui.importError }) : null,
      el(
        "div",
        { className: "first-run-row" },
        el("button", {
          type: "button",
          className: "slab ghost",
          textContent: "Browse...",
          title: "Search another folder, or paste a .code-workspace path",
          onclick: () => {
            openWorkspaceDialog();
            const toggle = $("#import-toggle");
            if (toggle && toggle.getAttribute("aria-expanded") !== "true") toggle.click();
          },
        }),
        pending.length > 1 ? el("button", {
          type: "button",
          className: "slab",
          textContent: `Import all ${pending.length}`,
          title: "One workspace per file",
          disabled: ui.busy,
          onclick: () => importAll(pending),
        }) : null,
        found ? el("button", {
          type: "button",
          className: "slab ghost",
          textContent: "Look again",
          disabled: ui.scanning,
          onclick: () => scan(),
        }) : null,
      ),
    );
  }

  function folderCard() {
    const input = el("input", {
      type: "text",
      id: "first-run-folder",
      placeholder: "~/code/project",
      autocomplete: "off",
      spellcheck: false,
      "aria-label": "Folder path",
      disabled: ui.busy,
    });
    const form = el(
      "form",
      {
        className: "first-run-row",
        onsubmit: (e) => {
          e.preventDefault();
          createFromFolder(input.value);
        },
      },
      input,
      el("button", { type: "submit", className: "slab", disabled: ui.busy, textContent: "Create" }),
    );
    return el(
      "section",
      { className: "first-run-card", "aria-labelledby": "first-run-folder-title" },
      el("h2", { id: "first-run-folder-title" }, icon("plus"), "Start from a folder"),
      el("p", { textContent: "A git repo or any directory; add more folders to the workspace later." }),
      form,
      ui.folderError ? el("p", { className: "first-run-note bad", role: "alert", textContent: ui.folderError }) : null,
      el("button", {
        type: "button",
        className: "first-run-link",
        textContent: "Several folders, or a name of your own...",
        onclick: () => openWorkspaceDialog(),
      }),
    );
  }

  function tourCard() {
    return el(
      "section",
      { className: "first-run-card first-run-tour", "aria-labelledby": "first-run-tour-title" },
      el("h2", { id: "first-run-tour-title" }, icon("wheel"), "The keys, in 30 seconds"),
      el("p", { textContent: "Six shortcuts that cover most of the hub. Every one is rebindable in Settings." }),
      el(
        "div",
        { className: "first-run-row" },
        el("button", { type: "button", className: "slab", textContent: "Take the tour", onclick: () => startTour() }),
        el("button", {
          type: "button",
          className: "slab ghost",
          textContent: "No thanks",
          title: "Settings > Keyboard > Shortcut tour brings it back",
          onclick: () => dismissTour(),
        }),
      ),
    );
  }

  // The start screen's content for a hub with no workspace (into `root`),
  // rebuilt only when what it shows changed (the start screen repaints
  // on every push), keeping a half-typed folder path.
  let shownKey = "";
  function render(root) {
    if (!ui.scan && !ui.scanning && !ui.scanError) scan();
    const tour = tourOffered() && !touchOnly.matches;
    const key = JSON.stringify([ui.scan, ui.scanning, ui.scanError, ui.busy, ui.folderError, ui.importError, tour]);
    if (key === shownKey && root.childElementCount) return;
    shownKey = key;
    const typed = root.querySelector("#first-run-folder");
    const value = typed ? typed.value : "";
    const hadFocus = typed && document.activeElement === typed;
    // replaceChildren takes nodes only (a null would show as "null").
    root.replaceChildren(importCard(), folderCard(), ...(tour ? [tourCard()] : []));
    const input = root.querySelector("#first-run-folder");
    if (input) {
      input.value = value;
      if (hadFocus) input.focus();
    }
  }

  // --- the tour ---------------------------------------------------------------

  // `keys`: the actions whose bindings in force show (each one's first).
  const STEPS = [
    {
      keys: ["palette.open"],
      title: "Command palette",
      body: "Every action, session, and workspace by name: start typing (\"Share\" makes a read-only live link to a session, \"Record\" saves it as a .cast). Press the key again while the palette is open to send it to the terminal instead.",
    },
    {
      keys: ["session.new"],
      title: "New session",
      body: "A shell, Claude Code, or Codex in one of a workspace's folders -- optionally in a fresh sibling git worktree.",
    },
    {
      keys: ["session.jump", "session.next"],
      title: "Jump between sessions",
      body: "With a number, the nth session in the sidebar; the arrow steps through them. The sidebar's dots tell each agent's state: working, blocked, done.",
    },
    {
      keys: ["pane.split_right", "pane.split_down"],
      title: "Split panes",
      body: "Split the focused pane right or down, each a new shell; tabs hold layouts. Prefer tmux keys? Turn on the pane prefix in Settings > Keyboard.",
    },
    {
      keys: ["search.open"],
      title: "Search everything",
      body: "Every terminal's scrollback and every agent transcript, across sessions.",
    },
    {
      keys: ["commands.toggle"],
      title: "Commands an agent suggested",
      body: "The shell blocks from an agent's replies, exactly as written: copy one, or send it to a shell.",
    },
  ];

  const tour = { dialog: null, step: 0, timer: 0, started: 0, left: STEP_MS, paused: false };

  function keyText(step) {
    const bound = step.keys.map((id) => ctx.keyOf(id)).filter(Boolean);
    return bound.length ? bound.join("  /  ") : "unbound (Settings > Keyboard)";
  }

  function buildTour() {
    const dialog = el("dialog", {
      id: "dlg-tour",
      className: "sheet narrow tour",
      "aria-labelledby": "tour-title",
      "aria-describedby": "tour-body",
    });
    dialog.addEventListener("close", () => stopTimer());
    dialog.addEventListener("cancel", (e) => {
      e.preventDefault();
      endTour(false);
    });
    // Over the card (not its backdrop, which is the dialog's too).
    dialog.addEventListener("pointermove", (e) => {
      const r = dialog.getBoundingClientRect();
      pause(e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom);
    });
    dialog.addEventListener("pointerleave", () => pause(false));
    dialog.addEventListener("focusin", (e) => {
      if (e.target.matches("button") && e.target.dataset.nav !== "auto") pause(true);
    });
    document.body.append(dialog);
    return dialog;
  }

  function renderStep() {
    const step = STEPS[tour.step];
    const last = tour.step === STEPS.length - 1;
    tour.dialog.replaceChildren(
      el(
        "div",
        { className: "sheet-head tour-head" },
        el("span", { className: "sheet-kicker", textContent: `shortcut ${tour.step + 1} of ${STEPS.length}` }),
        el("button", {
          type: "button",
          className: "tour-close",
          title: "Close the tour (Esc)",
          "aria-label": "Close the tour",
          onclick: () => endTour(false),
        }, icon("x")),
      ),
      el("kbd", { className: "tour-key", textContent: keyText(step) }),
      el("h2", { id: "tour-title", textContent: step.title }),
      el("p", { id: "tour-body", textContent: step.body }),
      el("div", { className: "tour-progress", "aria-hidden": "true" }, el("span", { className: "tour-bar" })),
      el(
        "div",
        { className: "tour-foot" },
        el(
          "ol",
          { className: "tour-dots", "aria-label": "Steps" },
          STEPS.map((s, i) => el("li", {
            className: i === tour.step ? "on" : i < tour.step ? "seen" : "",
            "aria-current": i === tour.step ? "step" : null,
            title: s.title,
          })),
        ),
        el("button", {
          type: "button",
          className: "slab ghost",
          textContent: "Back",
          disabled: tour.step === 0,
          onclick: () => go(tour.step - 1),
        }),
        el("button", {
          type: "button",
          className: "slab",
          dataset: { nav: "auto" },
          textContent: last ? "Done" : "Next",
          onclick: () => (last ? endTour(true) : go(tour.step + 1)),
        }),
      ),
    );
    const next = tour.dialog.querySelector('[data-nav="auto"]');
    if (next) next.focus();
    restartTimer();
  }

  function go(step) {
    tour.step = Math.max(0, Math.min(STEPS.length - 1, step));
    renderStep();
  }

  function stopTimer() {
    clearTimeout(tour.timer);
    tour.timer = 0;
  }

  // The bar shows the step's time; it runs while not paused.
  function restartTimer() {
    stopTimer();
    tour.left = STEP_MS;
    tour.paused = false;
    runTimer();
  }

  function runTimer() {
    const bar = tour.dialog && tour.dialog.querySelector(".tour-bar");
    if (!bar || tour.paused) return;
    tour.started = performance.now();
    const done = 1 - tour.left / STEP_MS;
    bar.style.transition = "none";
    bar.style.transform = `scaleX(${done})`;
    // Next frame: animate the rest of the step.
    requestAnimationFrame(() => {
      bar.style.transition = `transform ${tour.left}ms linear`;
      bar.style.transform = "scaleX(1)";
    });
    tour.timer = setTimeout(() => {
      if (tour.step === STEPS.length - 1) endTour(true);
      else go(tour.step + 1);
    }, tour.left);
  }

  function pause(on) {
    if (!tour.dialog || !tour.dialog.open || tour.paused === on) return;
    const bar = tour.dialog.querySelector(".tour-bar");
    if (on) {
      stopTimer();
      tour.left = Math.max(0, tour.left - (performance.now() - tour.started));
      tour.paused = true;
      if (bar) {
        const done = 1 - tour.left / STEP_MS;
        bar.style.transition = "none";
        bar.style.transform = `scaleX(${done})`;
      }
    } else {
      tour.paused = false;
      runTimer();
    }
  }

  function startTour() {
    if (!tour.dialog) tour.dialog = buildTour();
    tour.step = 0;
    if (!tour.dialog.open) tour.dialog.showModal();
    renderStep();
  }

  // Seen to the end or closed: either way it is not offered again.
  function endTour(finished) {
    stopTimer();
    if (tour.dialog && tour.dialog.open) tour.dialog.close();
    if (tourOffered()) dismissTour(finished ? "Tour done." : null);
    ctx.focusTerminal();
  }

  async function dismissTour(message) {
    await prefs.set(TOUR_PREF, false);
    if (message) toast(`${message} Settings > Keyboard > Shortcut tour offers it again.`, { timeout: 4000 });
    ctx.rerender();
  }

  function paletteItems(cmd) {
    cmd("Shortcut tour", "the 30-second tour of the key shortcuts", () => startTour(), "gear");
  }

  return { render, startTour, paletteItems, isTourOpen: () => !!(tour.dialog && tour.dialog.open) };
};

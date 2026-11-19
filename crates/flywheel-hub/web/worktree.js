// flywheel hub: sessions in sibling git worktrees. The new-session form's
// "new worktree" fields (GET /api/worktrees/plan checks as you type), the
// branch / changes / ahead badge under a session row, and the close
// dialog that keeps or removes a worktree (GET /api/sessions/{id}/worktree,
// DELETE /api/sessions/{id}?remove_worktree=true[&force=true]). app-base.js
// creates it with its helpers.
"use strict";

window.FwHubWorktree = function (ctx) {
  const { $, el, icon, api } = ctx;

  const box = $("#wt-pick");
  const toggle = $("#wt-on");
  const fields = $("#wt-fields");
  const taskInput = $("#wt-task");
  const baseInput = $("#wt-base");
  const bases = $("#wt-bases");
  const note = $("#wt-note");
  const launch = $("#session-submit");
  const launchError = $("#session-error");

  const form = {
    folder: null, // the picked folder {path, git, missing}
    seq: 0, // drops answers to superseded checks
    timer: null,
    defaultBase: "",
    blocked: false, // the last check found the worktree cannot be cut
  };

  const { plural, pathTail } = window.FwHubFmt;

  // `content`: text, or nodes.
  function setNote(content, tone) {
    note.replaceChildren(...(Array.isArray(content) ? content : content ? [content] : []));
    note.className = `wt-note${tone ? ` ${tone}` : ""}`;
  }

  // A worktree path as a line of its own, cut from its start.
  const pathLine = (path) => el("code", { className: "path-tail", textContent: pathTail(path), title: path });

  // Launch is held while the live check shows the worktree cannot be cut.
  function setBlocked(blocked) {
    form.blocked = !!blocked;
    launch.disabled = form.blocked;
  }

  // The user changed a worktree field: an earlier Launch error is stale.
  function edited() {
    launchError.textContent = "";
  }

  function showFields() {
    fields.hidden = !toggle.checked;
    box.classList.toggle("on", toggle.checked);
  }

  // The folder picked in the form changed (or the form opened).
  function setFolder(folder) {
    const changed = !form.folder || !folder || form.folder.path !== folder.path;
    form.folder = folder || null;
    const git = !!(folder && folder.git && !folder.missing);
    toggle.disabled = !git;
    box.classList.toggle("unavailable", !git);
    if (!git) toggle.checked = false;
    showFields();
    if (!git) {
      setBlocked(false);
      setNote(folder ? "Worktrees need a git folder; this one is a plain directory." : "", "");
      return;
    }
    if (changed) {
      bases.replaceChildren();
      form.defaultBase = "";
    }
    if (toggle.checked || changed) schedule(0);
  }

  function reset() {
    toggle.checked = false;
    taskInput.value = "";
    baseInput.value = "";
    form.folder = null;
    setNote("", "");
    setBlocked(false);
    showFields();
  }

  function schedule(delay) {
    clearTimeout(form.timer);
    form.timer = setTimeout(check, delay);
  }

  async function check() {
    const folder = form.folder;
    if (!folder || !folder.git) return;
    const seq = ++form.seq;
    const params = new URLSearchParams({ folder: folder.path });
    const task = taskInput.value.trim();
    const base = baseInput.value.trim();
    if (toggle.checked && task) params.set("task", task);
    if (base) params.set("base", base);
    let view;
    try {
      view = await api("GET", `/api/worktrees/plan?${params}`);
    } catch (error) {
      if (seq === form.seq) {
        setNote(error.message, "bad");
        setBlocked(toggle.checked);
      }
      return;
    }
    if (seq !== form.seq) return;
    setBlocked(toggle.checked && !!view.error);
    form.defaultBase = view.default_base || "";
    baseInput.placeholder = form.defaultBase ? `${form.defaultBase} (current branch)` : "HEAD";
    bases.replaceChildren(...(view.branches || []).map((b) => el("option", { value: b })));
    if (!toggle.checked) {
      setNote(view.dir ? ["Sibling worktrees go in", pathLine(`${view.dir}/`)] : view.error || "", view.dir ? "" : "bad");
    } else if (view.plan) {
      setNote([pathLine(view.plan.path), `on new branch ${view.plan.branch}, from ${view.plan.base}`], "ok");
    } else if (view.error) {
      setNote(view.error, "bad");
    } else {
      setNote(["Name a task: it runs in", pathLine(`${view.dir}/<task>`), "on a new branch of that name."], "");
    }
  }

  toggle.addEventListener("change", () => {
    edited();
    showFields();
    if (!toggle.checked) setBlocked(false);
    if (toggle.checked) taskInput.focus();
    schedule(0);
  });
  taskInput.addEventListener("input", () => {
    edited();
    schedule(180);
  });
  baseInput.addEventListener("input", () => {
    edited();
    schedule(250);
  });

  // The request's `worktree` (null when off); throws when it cannot be.
  function request() {
    if (!toggle.checked || toggle.disabled) return null;
    const task = taskInput.value.trim();
    if (!task) {
      taskInput.focus();
      throw new Error("Name the worktree's task: it names the directory and the branch.");
    }
    const base = baseInput.value.trim();
    return base ? { task, base } : { task };
  }

  // Turn the worktree fields on with `task` (a template that runs in a
  // worktree), or off. False when the picked folder cannot have one.
  function use(on, task) {
    if (on && toggle.disabled) return false;
    toggle.checked = !!on;
    if (on && task) taskInput.value = task;
    edited();
    showFields();
    if (!on) setBlocked(false);
    schedule(0);
    return true;
  }

  const taskName = () => (toggle.checked ? taskInput.value.trim() : "");
  const blocked = () => form.blocked;

  // --- badges --------------------------------------------------------------

  // The branch line under a session row: branch, ahead/behind its base or
  // upstream, changed and untracked files (from the hub's cache).
  function badge(session) {
    const git = session.git;
    const worktree = session.worktree;
    if (!git && !worktree) return null;
    const branch = git ? git.branch || "detached" : worktree.branch;
    const counts = [];
    const tips = [];
    if (git) {
      if (git.ahead) {
        counts.push(el("span", { className: "ahead", textContent: `↑${git.ahead}` }));
        tips.push(`${plural(git.ahead, "commit")} ahead of ${git.against}`);
      }
      if (git.behind) {
        counts.push(el("span", { className: "behind", textContent: `↓${git.behind}` }));
        tips.push(`${git.behind} behind ${git.against}`);
      }
      if (git.dirty) {
        counts.push(el("span", { className: "dirty", textContent: `~${git.dirty}` }));
        tips.push(plural(git.dirty, "changed file"));
      }
      if (git.untracked) {
        counts.push(el("span", { className: "untracked", textContent: `?${git.untracked}` }));
        tips.push(plural(git.untracked, "untracked file"));
      }
      if (!counts.length) tips.push(git.against ? `clean, even with ${git.against}` : "clean");
    }
    const where = worktree ? `worktree ${worktree.path} (from ${worktree.base})` : "";
    return el(
      "span",
      {
        className: `session-git${worktree ? " worktree" : ""}${git && (git.dirty || git.untracked) ? " dirty" : ""}`,
        title: [`branch ${branch}`, ...tips, where].filter(Boolean).join("\n"),
      },
      icon("branch"),
      el("span", { className: "branch", textContent: branch }),
      worktree ? el("span", { className: "wt-tag", textContent: "wt" }) : null,
      ...counts,
    );
  }

  // --- close dialog --------------------------------------------------------

  const dialog = $("#dlg-close-wt");
  const removeButton = $("#cwt-remove");
  const forceBox = $("#cwt-force");
  const forceRow = $("#cwt-force-row");
  const state = $("#cwt-state");
  const also = $("#cwt-also");
  let info = null;

  function syncRemove() {
    const blocked = info && info.blocking && info.blocking.length;
    const needsForce = info && info.refusal;
    removeButton.disabled = !info || !!blocked || (needsForce && !forceBox.checked);
    removeButton.textContent = needsForce && forceBox.checked ? "Force remove worktree" : "Remove worktree";
  }

  forceBox.addEventListener("change", syncRemove);

  // Ask how to close a worktree session: null (cancel), or {remove, force}.
  function confirmClose(session) {
    const worktree = session.worktree;
    info = null;
    $("#cwt-title").textContent = `Close ${session.name}?`;
    $("#cwt-where").replaceChildren(
      "Runs in worktree ",
      pathLine(worktree.path),
      "on branch ",
      el("code", { textContent: worktree.branch }),
      ` (from ${worktree.base}).`,
      session.state === "running" && session.live ? " Its process is hung up either way." : "",
    );
    state.textContent = "Checking the worktree…";
    state.className = "cwt-state";
    also.textContent = "";
    forceBox.checked = false;
    forceRow.hidden = true;
    syncRemove();
    dialog.returnValue = "";
    dialog.showModal();
    $("#cwt-keep").focus();
    const id = session.id;
    api("GET", `/api/sessions/${id}/worktree`).then(
      (answer) => {
        if (!dialog.open) return;
        info = answer;
        const parts = [];
        if (answer.missing) parts.push("Its directory is already gone; removing prunes git's record.");
        if (answer.blocking.length) {
          parts.push(`Cannot remove: ${answer.blocking.join(", ")} also ${answer.blocking.length === 1 ? "uses" : "use"} it. Close ${answer.blocking.length === 1 ? "it" : "them"} first.`);
        } else if (answer.refusal) {
          parts.push(answer.refusal.replace(/; commit and merge.*$/, "."));
        } else {
          parts.push(`Clean, and ${answer.branch} has nothing ${answer.base} lacks: removing deletes the worktree and the branch.`);
        }
        state.textContent = parts.join(" ");
        also.textContent = answer.companions.length
          ? `Closing ${session.name} also closes ${answer.companions.join(", ")}, either way.`
          : "";
        state.className = `cwt-state${answer.blocking.length || answer.refusal ? " bad" : " ok"}`;
        forceRow.hidden = !answer.refusal || !!answer.blocking.length;
        syncRemove();
      },
      (error) => {
        if (!dialog.open) return;
        state.textContent = `Could not check the worktree: ${error.message}`;
        state.className = "cwt-state bad";
      },
    );
    return new Promise((resolve) => {
      dialog.addEventListener(
        "close",
        () => {
          const how = dialog.returnValue;
          if (how === "keep") resolve({ remove: false, force: false });
          else if (how === "remove") resolve({ remove: true, force: forceBox.checked });
          else resolve(null);
        },
        { once: true },
      );
    });
  }

  // The close checklist (GET /api/sessions/{id}/checklist: merged?,
  // untracked files and sizes, target/ size, who else works there) and
  // its choices, run as `fw hub close` runs them (POST
  // /api/sessions/{id}/close: ended at rest, cleanups that lose nothing).
  // Resolves {closed, forget}, {kill, remove} (it was busy and the user
  // chose to hang it up), or null.
  const cl = $("#dlg-close");
  const size = (bytes, partial) => {
    const mib = bytes / (1024 * 1024);
    const text = mib >= 102.4 ? `${(mib / 1024).toFixed(1)} GiB`
      : mib >= 1 ? `${Math.round(mib)} MiB`
        : bytes >= 1024 ? `${Math.round(bytes / 1024)} KiB` : `${bytes} B`;
    return partial ? `at least ${text}` : text;
  };

  function checklistRows(list) {
    const rows = [];
    const row = (text, bad) => rows.push(el("li", { className: bad ? "bad" : "" }, ...[].concat(text)));
    rows.push(el("li", { className: "cl-where" }, pathLine(list.path), list.branch ? el("code", { textContent: list.branch }) : null));
    if (list.merged && list.base) {
      const m = list.merged;
      row(m.state === "ancestor" ? `merged into ${list.base}`
        : m.state === "patch_equivalent" ? `merged into ${list.base} by patch (${m.commits} squashed or rebased in); the branch is kept`
          : `not merged: ${m.commits} commit(s) ${list.base} lacks; the branch is kept`, m.state === "unmerged");
    }
    row(`${list.dirty} uncommitted change(s), ${list.untracked_count} untracked`, list.dirty + list.untracked_count > 0);
    for (const u of list.untracked || []) {
      const rel = u.path.startsWith(list.path) ? u.path.slice(list.path.length).replace(/^\//, "") : u.path;
      row(`  untracked: ${rel} (${size(u.bytes, u.partial)})`, true);
    }
    row(list.build ? `build output target/: ${size(list.build.bytes, list.build.partial)}` : "no build output to clean");
    if (list.shared_with && list.shared_with.length) row(`also used by ${list.shared_with.join(", ")}`, true);
    return rows;
  }

  function closeChecklist(session, change) {
    const id = session.id;
    // An adopted worktree (a change) closes through its own routes.
    const readList = change
      ? () => api("POST", `/api/changes/${encodeURIComponent(change.id)}/close`, { dry_run: true })
      : () => api("GET", `/api/sessions/${encodeURIComponent(id)}/checklist`);
    const closeUrl = change
      ? `/api/changes/${encodeURIComponent(change.id)}/close`
      : `/api/sessions/${encodeURIComponent(id)}/close`;
    $("#cl-title").textContent = `Close ${session.name}?`;
    $("#cl-list").replaceChildren(el("li", { textContent: "Checking…" }));
    const wtKeep = $("#cl-wt-keep");
    const wtRemove = $("#cl-wt-remove");
    const buildKeep = $("#cl-build-keep");
    const buildClean = $("#cl-build-clean");
    const report = $("#cl-report");
    const forget = $("#cl-forget");
    const status = $("#cl-state");
    const ok = $("#cl-ok");
    const kill = $("#cl-kill");
    wtKeep.checked = true;
    buildKeep.checked = true;
    wtRemove.disabled = true;
    buildClean.disabled = true;
    report.value = "";
    forget.checked = !change;
    forget.parentElement.hidden = !!change;
    status.textContent = session.state === "running" && session.live
      ? "It ends once at rest (never mid-turn or mid tool call); then the choices run."
      : "";
    status.className = "cwt-state";
    ok.disabled = true;
    kill.hidden = true;
    $("#cl-wt-why").textContent = "";
    $("#cl-build-why").textContent = "";
    let settled = null;
    readList().then(
      (answer) => {
        if (!cl.open) return;
        const list = answer.checklist;
        $("#cl-list").replaceChildren(...checklistRows(list));
        wtRemove.disabled = !!list.cannot_remove;
        $("#cl-wt-why").textContent = list.cannot_remove
          ? list.cannot_remove
          : list.removes_branch ? "its branch is deleted too (fully merged)" : "its branch is kept";
        buildClean.disabled = !!list.cannot_clean;
        // Its own path is the dialog's first line: not again here.
        $("#cl-build-why").textContent = (list.cannot_clean || "").split(`${list.path}/`).join("").split(list.path).join("it");
        ok.disabled = false;
      },
      (error) => {
        if (!cl.open) return;
        status.textContent = `Could not read its checklist: ${error.message}`;
        status.className = "cwt-state bad";
        ok.disabled = false;
      },
    );
    ok.onclick = async () => {
      ok.disabled = true;
      status.textContent = "Closing…";
      status.className = "cwt-state";
      try {
        const done = await api("POST", closeUrl, {
          worktree: wtRemove.checked ? "remove" : "keep",
          build: buildClean.checked ? "clean" : "keep",
          report: report.value.trim() || null,
        });
        settled = { closed: true, forget: forget.checked, text: done.text };
        cl.close("done");
      } catch (error) {
        status.textContent = `${error.message}`;
        status.className = "cwt-state bad";
        // Busy: it can still be hung up the old way.
        kill.hidden = !/at rest/.test(error.message);
        ok.disabled = false;
      }
    };
    kill.onclick = () => {
      settled = { kill: true, remove: wtRemove.checked };
      cl.close("kill");
    };
    cl.returnValue = "";
    cl.showModal();
    return new Promise((resolve) => {
      cl.addEventListener("close", () => resolve(settled), { once: true });
    });
  }

  // Adopted worktrees (changes) of a workspace and the ones nothing
  // tracks (GET /api/changes?untracked=1, read at most once a minute):
  // a row each, Close for a change, Adopt for an untracked one.
  let changesSeen = { at: 0, view: null, asking: false };
  function readChanges(rerender, now) {
    if (changesSeen.asking || (!now && Date.now() - changesSeen.at < 60000)) return;
    changesSeen.asking = true;
    api("GET", "/api/changes?untracked=1").then(
      (view) => {
        changesSeen = { at: Date.now(), view, asking: false };
        rerender();
      },
      () => {
        changesSeen = { at: Date.now(), view: changesSeen.view, asking: false };
      },
    );
  }

  function changesBlock(workspace, rerender) {
    readChanges(rerender, false);
    const view = changesSeen.view;
    if (!view) return null;
    const folders = workspace.folders.map((f) => f.path);
    const mine = (repo) => folders.includes(repo);
    const changes = view.changes.filter((c) => mine(c.worktree.repo));
    const untracked = view.untracked.filter((u) => mine(u.repo));
    if (!changes.length && !untracked.length) return null;
    const changeRow = (c) => {
      const k = c.check || {};
      return el("li", { className: "ws-change", dataset: { change: c.id }, title: c.worktree.path },
        el("code", { textContent: c.worktree.branch }),
        el("span", { className: "muted", textContent: `${k.dirty || 0} dirty · ${k.untracked || 0} untracked · ${k.unmerged || 0} unmerged${c.session_name ? ` · ${c.session_name}` : ""}` }),
        el("button", {
          type: "button",
          className: "text-btn",
          textContent: "close",
          onclick: async () => {
            const how = await closeChecklist({ id: c.id, name: c.worktree.branch, state: "exited" }, c);
            if (how) readChanges(rerender, true);
          },
        }));
    };
    const untrackedRow = (u) =>
      el("li", { className: "ws-change", title: u.path },
        el("code", { textContent: u.branch || "detached" }),
        el("span", { className: "muted", textContent: pathTail(u.path) }),
        u.branch
          ? el("button", {
            type: "button",
            className: "text-btn",
            textContent: "adopt",
            onclick: async (event) => {
              event.currentTarget.disabled = true;
              try {
                await api("POST", "/api/changes/adopt", { path: u.path });
              } finally {
                readChanges(rerender, true);
              }
            },
          })
          : el("span", { className: "muted", textContent: "check out a branch to adopt" }));
    return el("div", { className: "ws-changes" },
      changes.length ? el("ul", { className: "ws-change-list", "aria-label": "Adopted worktrees" }, changes.map(changeRow)) : null,
      untracked.length
        ? el("details", { className: "ws-untracked" },
          el("summary", { textContent: `Not tracked (${untracked.length})` }),
          el("ul", { className: "ws-change-list" }, untracked.map(untrackedRow)))
        : null);
  }

  return { setFolder, reset, request, use, taskName, blocked, badge, confirmClose, closeChecklist, changesBlock };
};

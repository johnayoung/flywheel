// flywheel hub: sessions in sibling git worktrees. The new-session form's
// "new worktree" fields (GET /api/worktrees/plan checks as you type), the
// branch / changes / ahead badge under a session row, and the close
// dialog that keeps or removes a worktree (GET /api/sessions/{id}/worktree,
// DELETE /api/sessions/{id}?remove_worktree=true[&force=true]). app.js
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

  const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

  function setNote(text, tone) {
    note.textContent = text || "";
    note.className = `wt-note${tone ? ` ${tone}` : ""}`;
  }

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
      setNote(view.dir ? `Sibling worktrees go in ${view.dir}/` : view.error || "", view.dir ? "" : "bad");
    } else if (view.plan) {
      setNote(`${view.plan.path}  on new branch ${view.plan.branch}, from ${view.plan.base}`, "ok");
    } else if (view.error) {
      setNote(view.error, "bad");
    } else {
      setNote(`Name a task: ${view.dir}/<task> on a new branch of that name.`, "");
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
      el("code", { textContent: worktree.path }),
      " on branch ",
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

  return { setFolder, reset, request, taskName, blocked, badge, confirmClose };
};

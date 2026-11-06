// flywheel hub web client: the confirm, workspace, and new-session
// dialogs.
"use strict";

// --- dialogs: confirm ------------------------------------------------

// `tone: "plain"` for a confirm that destroys nothing (an approval): its
// button is not styled as a danger.
function confirmDialog({ kicker, title, body, ok, tone }) {
  const dialog = $("#dlg-confirm");
  $("#confirm-kicker").textContent = kicker || "confirm";
  $("#confirm-title").textContent = title;
  $("#confirm-body").textContent = body;
  $("#confirm-ok").textContent = ok || "Confirm";
  $("#confirm-ok").classList.toggle("danger", tone !== "plain");
  dialog.returnValue = "";
  // Last in the document, as it is in the top layer: opened over another
  // modal (the Commit dialog's "Ask the agent"), tools that take the
  // topmost modal from document order (Playwright, some assistive tech)
  // otherwise treat its buttons as inert.
  if (document.body.lastElementChild !== dialog) document.body.append(dialog);
  dialog.showModal();
  $("#confirm-ok").focus();
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => resolve(dialog.returnValue === "ok"), { once: true });
  });
}

// Close buttons inside dialogs, and focus return to the terminal.
document.querySelectorAll("dialog").forEach((dialog) => {
  dialog.addEventListener("click", (event) => {
    if (event.target.closest("[data-close]")) dialog.close("cancel");
    // A click on the backdrop lands on the dialog element itself.
    if (event.target === dialog) dialog.close("cancel");
  });
  dialog.addEventListener("close", () => returnFocus());
});

// Where keys go once a dialog closes: back into the drawer it was opened
// from while that drawer stays open (keys typed into the terminal behind
// it would go unseen, and Esc must close the drawer), else the terminal.
function returnFocus() {
  if (document.querySelector("dialog[open]")) return;
  const drawer = review.isOpen() ? $("#review") : flywheel.isOpen() ? $("#flywheel") : commands.isOpen() ? $("#commands") : null;
  if (drawer) drawer.focus({ preventScroll: true });
  else focusTerminal();
}

// --- dialogs: workspace ----------------------------------------------

const wsForm = $("#workspace-form");
// "Import VS Code workspace" (import.js), inside this dialog.
const importer = window.FwHubImport({ $, el, api, refresh, rememberWorkspace, basename, workspaceById });
// The first run (onboarding.js): the start screen's ways in, the tour.
const onboarding = window.FwHubOnboarding({
  $, el, icon, api, refresh, rememberWorkspace, basename,
  prefs: settings,
  openWorkspaceDialog: () => openWorkspaceDialog(),
  toast: (...args) => commands.toast(...args),
  keyOf: (id) => settings.keyOf(id),
  rerender: () => renderMain(),
  focusTerminal: () => focusTerminal(),
});
settings.onChange((ids) => {
  if (ids.includes("keyboard.tour")) renderMain();
});
let wsEditing = null; // workspace being edited, or null for create
let checkSeq = 0;
// Completion, the folder browser, and suggestions (folders.js), over the
// dialog's machine's directories.
const folders = window.FwHubFolders({
  $, el, icon, api, basename,
  rows: () => [...$("#repo-rows").children],
  machine: () => (wsEditing ? wsEditing.machine || null : $("#ws-machine").value || null),
  addFolders: (paths, origin) => addFolders(paths, origin),
});

// `value`: a path, or a workspace folder {path, name}.
function repoRow(value) {
  const folder = typeof value === "object" && value ? value : { path: value || "" };
  const input = el("input", {
    type: "text",
    value: folder.path || "",
    placeholder: "~/code/project",
    autocomplete: "off",
    spellcheck: false,
    "aria-label": "Folder path",
  });
  const label = el("input", {
    type: "text",
    className: "folder-label",
    value: folder.name || "",
    placeholder: "label",
    maxLength: 100,
    autocomplete: "off",
    spellcheck: false,
    "aria-label": "Folder label (optional; shown instead of the directory name)",
    title: "Label (optional): shown instead of the directory name",
  });
  const browse = iconButton("folder", "Browse folders on the hub's machine", () => folders.browse(row));
  browse.classList.add("browse");
  const verdict = icon("check");
  verdict.classList.add("verdict");
  const note = el("span", { className: "note" });
  const row = el(
    "li",
    { className: "repo-row", dataset: { state: "empty" } },
    el("span", { className: "input-wrap" }, input, verdict),
    browse,
    label,
    iconButton("x", "Remove folder", () => {
      row.remove();
      if (!$("#repo-rows").children.length) addRepoRow("");
      markDuplicates();
    }),
    note,
  );
  folders.attach(input);
  let timer = 0;
  row.check = () => checkRow(row);
  input.addEventListener("input", () => {
    clearTimeout(timer);
    row.dataset.state = input.value.trim() ? "checking" : "empty";
    setVerdict(row, true);
    // Drop the previous check too: its answer is for the old text.
    row.result = null;
    row.pending = null;
    row.seq = ++checkSeq;
    note.textContent = "";
    timer = setTimeout(row.check, 280);
  });
  input.addEventListener("paste", (event) => {
    const text = (event.clipboardData || window.clipboardData).getData("text");
    const lines = text.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
    if (lines.length < 2) return;
    event.preventDefault();
    input.value = lines[0];
    input.dispatchEvent(new Event("input"));
    let after = row;
    for (const line of lines.slice(1)) {
      const next = repoRow(line);
      after.after(next);
      after = next;
      next.check();
    }
  });
  row.input = input;
  row.label = label;
  row.note = note;
  if (folder.path) {
    row.dataset.state = "checking";
    row.check();
  }
  return row;
}

function addRepoRow(value) {
  const row = repoRow(value);
  $("#repo-rows").append(row);
  return row;
}

// Add folder paths (from the browser or a suggestion): the first fills
// `origin` (the row the browser opened from) when it is empty -- with no
// origin, the first empty row -- the rest become rows after it. A folder
// already listed is skipped, and the toast says so.
function addFolders(paths, origin) {
  const rows = [...$("#repo-rows").children];
  // "Select this folder" from a row's browser replaces what the row says.
  let empty = origin && (!origin.input.value.trim() || paths.length === 1) ? origin : null;
  if (!origin) empty = rows.find((row) => !row.input.value.trim()) || null;
  const listed = new Set();
  for (const row of rows) {
    if (row === empty) continue;
    if (row.result && row.result.ok) listed.add(row.result.path);
    if (row.input.value.trim()) listed.add(folders.expand(row.input.value));
  }
  let after = origin || rows[rows.length - 1] || null;
  let skipped = 0;
  let last = null;
  for (const path of paths) {
    const full = folders.expand(path);
    if (listed.has(full)) {
      skipped += 1;
      continue;
    }
    listed.add(full);
    if (empty) {
      empty.input.value = path;
      empty.input.dispatchEvent(new Event("input"));
      after = empty;
      last = empty;
      empty = null;
      continue;
    }
    const row = repoRow(path);
    if (after) after.after(row);
    else $("#repo-rows").append(row);
    after = row;
    last = row;
  }
  if (last) last.input.focus();
  markDuplicates();
  if (skipped) {
    commands.toast(skipped === 1 ? "That folder is already in the list." : `${skipped} folders were already in the list.`);
  }
}

async function checkRow(row) {
  const raw = row.input.value.trim();
  if (!raw) {
    row.dataset.state = "empty";
    row.result = null;
    return null;
  }
  const seq = ++checkSeq;
  row.seq = seq;
  row.dataset.state = "checking";
  row.pendingRaw = raw;
  row.pending = api("GET", `/api/folders/check?path=${encodeURIComponent(raw)}`)
    .then((result) => {
      if (row.seq !== seq) return row.pending;
      // A folder the workspace already has may stay while it is gone
      // from disk (the hub keeps it, flagged missing).
      const kept = wsEditing && wsEditing.folders.find((f) => f.path === raw);
      if (!result.ok && kept && kept.missing) {
        result = { ok: true, path: raw, git: false, missing: true };
      }
      row.result = { ...result, raw };
      row.dataset.state = result.ok ? "ok" : "bad";
      setVerdict(row, result.ok);
      row.note.textContent = result.ok ? okNote(result, raw) : result.error;
      markDuplicates();
      return result;
    })
    .catch((error) => {
      if (row.seq !== seq) return row.pending;
      row.result = { ok: false, error: error.message, raw };
      row.dataset.state = "bad";
      setVerdict(row, false);
      row.note.textContent = error.message;
      return row.result;
    });
  return row.pending;
}

// The check answer for exactly what the row says now: the settled
// result, the in-flight check, or a fresh one -- never an answer for
// text the user has since changed.
function currentCheck(row) {
  const raw = row.input.value.trim();
  if (row.result && row.result.raw === raw) return row.result;
  if (row.pending && row.pendingRaw === raw) return row.pending;
  return row.check();
}

// A good path's note: where it resolved to, and whether it is a plain
// folder (repo-only features such as worktrees will not apply).
function okNote(result, raw) {
  if (result.missing) return "missing on disk (moved or deleted?); kept until you remove it";
  const where = result.path !== raw ? result.path : "";
  if (result.git !== false) return where;
  return where ? `${where} · plain folder (no git)` : "plain folder (no git)";
}

function setVerdict(row, ok) {
  row.querySelector(".verdict use").setAttribute("href", ok ? "#i-check" : "#i-x");
}

function markDuplicates() {
  const seen = new Map();
  [...$("#repo-rows").children].forEach((row, i) => {
    if (!row.result || !row.result.ok) return;
    const first = seen.get(row.result.path);
    if (first !== undefined) {
      row.note.textContent = `same folder as ${pad2(first + 1)}; listed once`;
    } else {
      seen.set(row.result.path, i);
      if (row.note.textContent.startsWith("same folder as")) {
        row.note.textContent = okNote(row.result, row.input.value.trim());
      }
    }
  });
  folders.renderSuggestions();
}

function openWorkspaceDialog(workspace) {
  wsEditing = workspace || null;
  wsForm.reset();
  $("#workspace-error").textContent = "";
  $("#dlg-workspace-title").textContent = workspace ? `Edit ${workspace.name}` : "New workspace";
  $("#workspace-kicker").textContent = workspace ? "edit workspace" : "workspace";
  $("#workspace-submit").textContent = workspace ? "Save" : "Create";
  wsForm.name.value = workspace ? workspace.name : "";
  $("#repo-rows").replaceChildren();
  // Which machine it lives on (its folders are paths there).
  const field = $("#ws-machine-field");
  field.hidden = !machines.any();
  const select = $("#ws-machine");
  const current = workspace ? workspace.machine || "" : (machineOfView() || "");
  select.replaceChildren(
    el("option", { value: "", textContent: `${machines.host() || "this machine"} (this hub)` }),
    ...machines.names().map((name) => el("option", { value: name, textContent: name })),
  );
  select.value = current;
  select.disabled = !!workspace;
  if (workspace) workspace.folders.forEach((f) => addRepoRow(f));
  else addRepoRow("");
  importer.reset(!workspace);
  // VS Code import reads this machine's files: here only.
  $("#ws-import").hidden = !!workspace || !!select.value;
  folders.reset();
  $("#dlg-workspace").showModal();
  folders.loadSuggestions();
  wsForm.name.focus();
}

$("#add-repo").addEventListener("click", () => addRepoRow("").input.focus());
$("#browse-repo").addEventListener("click", () => {
  folders.browse([...$("#repo-rows").children].find((row) => !row.input.value.trim()) || null);
});
// Another machine: the folder paths are checked again, there, and its
// directories browsed and suggested.
$("#ws-machine").addEventListener("change", () => {
  $("#ws-import").hidden = !!$("#ws-machine").value;
  folders.reset();
  folders.loadSuggestions();
  for (const row of $("#repo-rows").children) if (row.input.value.trim()) checkRow(row);
});

// The machine of the workspace on the stage (a new workspace defaults to it).
const machineOfView = () => (workspaceById(state.view) || {}).machine || null;

// Calls without an id (kinds, folder checks, worktree plans, a new
// workspace) go to the machine the open dialog is about.
machines.setContext(() => {
  if ($("#dlg-workspace").open) return wsEditing ? wsEditing.machine || null : $("#ws-machine").value || null;
  if ($("#dlg-session").open) return (workspaceById($("#session-workspace").value) || {}).machine || null;
  return null;
});
// A stale error (name taken, bad path) clears as soon as the form changes.
wsForm.addEventListener("input", () => {
  $("#workspace-error").textContent = "";
});

wsForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const error = $("#workspace-error");
  error.textContent = "";
  const name = wsForm.name.value.trim();
  if (!name) {
    error.textContent = "Give the workspace a name.";
    wsForm.name.focus();
    return;
  }
  const rows = [...$("#repo-rows").children].filter((row) => row.input.value.trim());
  if (!rows.length) {
    error.textContent = "Add at least one folder.";
    $("#repo-rows .repo-row input").focus();
    return;
  }
  const submit = $("#workspace-submit");
  submit.disabled = true;
  try {
    const results = await Promise.all(rows.map(currentCheck));
    const bad = rows.find((row, i) => !results[i] || !results[i].ok);
    if (bad) {
      error.textContent = "Fix the highlighted folder paths.";
      bad.input.focus();
      return;
    }
    // An empty label clears it (the directory name shows).
    const folders = results.map((r, i) => ({ path: r.path, name: rows[i].label.value.trim() }));
    const saved = wsEditing
      ? await api("PATCH", `/api/workspaces/${wsEditing.id}`, { name, folders })
      : await api("POST", "/api/workspaces", { name, folders });
    rememberWorkspace(saved.id);
    $("#dlg-workspace").close("ok");
    await refresh();
  } catch (e) {
    error.textContent = e.message;
  } finally {
    submit.disabled = false;
  }
});

// --- dialogs: session ------------------------------------------------

const sessionForm = $("#session-form");

// What the dialog's machine can launch (each machine has its own).
const kindsKey = () => ($("#dlg-session").open ? (workspaceById($("#session-workspace").value) || {}).machine : null) || "";
async function loadKinds() {
  const key = kindsKey();
  state.kindsBy = state.kindsBy || {};
  try {
    state.kindsBy[key] = (await api("GET", "/api/kinds")).kinds;
  } catch (_) {
    state.kindsBy[key] = state.kindsBy[key] || [{ kind: "shell", available: true, program: null }];
  }
  if (!key) state.kinds = state.kindsBy[key];
  return state.kindsBy[key];
}

function selectedKind() {
  const checked = sessionForm.querySelector('input[name="kind"]:checked');
  return checked ? checked.value : "shell";
}

function renderKinds(kinds, preferred) {
  const usable = kinds.filter((k) => k.available).map((k) => k.kind);
  const pick = usable.includes(preferred) ? preferred : usable[0] || "shell";
  const cards = kinds.map((k) =>
    el(
      "label",
      { className: `kind-card ${k.kind}` },
      el("input", {
        type: "radio",
        name: "kind",
        value: k.kind,
        checked: k.kind === pick,
        disabled: !k.available,
        onchange: updateSessionPlaceholder,
      }),
      icon(glyphFor(k.kind)),
      el("span", { className: "kind-name", textContent: k.kind === "claude" ? "Claude" : k.kind === "codex" ? "Codex" : "Shell" }),
      el("span", {
        className: "kind-prog",
        textContent: k.available ? k.program || k.kind : "not installed",
        title: k.available ? (k.program || "") : `${k.kind} is not on the hub's PATH`,
      }),
    ));
  $("#kind-picker").replaceChildren(el("legend", { className: "field-label", textContent: "Runs" }), ...cards);
  launcher.setKind(selectedKind());
}
$("#kind-picker").addEventListener("change", () => launcher.setKind(selectedKind()));

function renderRepoPicker() {
  const workspace = workspaceById($("#session-workspace").value);
  const picker = $("#repo-picker");
  const legend = el("legend", { className: "field-label", textContent: "In folder" });
  if (!workspace) {
    picker.replaceChildren(legend);
    return;
  }
  // The folder of the session you are in, then the last one picked here.
  const attached = sessionById(state.attached);
  const paths = workspace.folders.map((f) => f.path);
  const chosen = [
    attached && attached.workspace_id === workspace.id ? (attached.worktree ? attached.worktree.repo : attached.cwd) : null,
    state.repoChoice[workspace.id],
  ].find((path) => path && paths.includes(path)) || paths[0];
  // Missing folders cannot be opened; they stay listed, disabled.
  const usable = workspace.folders.filter((f) => !f.missing).map((f) => f.path);
  const pick = usable.includes(chosen) ? chosen : usable[0];
  const choices = workspace.folders.map((folder) => {
    const tag = folder.missing ? "missing" : folder.git ? null : "no git";
    const choice = el(
      "label",
      { className: `repo-choice${folder.missing ? " missing" : ""}`, title: folder.path },
      el("input", {
        type: "radio",
        name: "repo",
        value: folder.path,
        checked: folder.path === pick,
        disabled: !!folder.missing,
      }),
      el("b", {}, folderLabel(folder), tag ? el("span", { className: "folder-tag", textContent: tag }) : null),
      el("small", { className: "path-tail", textContent: window.FwHubFmt.pathTail(folder.path) }),
    );
    choice.dataset.match = `${folderLabel(folder)} ${folder.path}`.toLowerCase();
    return choice;
  });
  const list = el("div", { className: "repo-list" }, choices);
  // Many folders (an imported workspace): a filter, and the list scrolls
  // so the Name field and Launch stay in view.
  const filter = workspace.folders.length > 5
    ? el("input", {
      type: "search",
      className: "repo-filter",
      placeholder: `filter ${workspace.folders.length} folders`,
      autocomplete: "off",
      spellcheck: false,
      "aria-label": "Filter folders",
      oninput: (event) => {
        const q = event.target.value.trim().toLowerCase();
        let first = null;
        for (const choice of choices) {
          const shown = !q || choice.dataset.match.includes(q);
          choice.hidden = !shown;
          const radio = choice.querySelector("input");
          if (shown && !radio.disabled && !first) first = radio;
        }
        // Keep the pick among what is shown.
        const checked = choices.find((c) => c.querySelector("input").checked);
        if (first && (!checked || checked.hidden)) first.checked = true;
      },
      onkeydown: (event) => {
        // Enter launches (the form submits); it must not clear the filter.
        if (event.key === "Escape" && event.target.value) {
          event.preventDefault();
          event.stopPropagation();
          event.target.value = "";
          event.target.dispatchEvent(new Event("input"));
        }
      },
    })
    : null;
  list.classList.toggle("long", !!filter);
  picker.replaceChildren(legend, ...(filter ? [filter] : []), list);
  pickedFolderChanged();
}

// The worktree fields follow the picked folder (git ones only).
function pickedFolderChanged() {
  const workspace = workspaceById($("#session-workspace").value);
  const checked = sessionForm.querySelector('input[name="repo"]:checked');
  const folder = workspace && checked ? workspace.folders.find((f) => f.path === checked.value) : null;
  worktrees.setFolder(folder || null);
  launcher.setFolder(folder ? folder.path : null);
  updateSessionPlaceholder();
}

// The name the hub gives a session started without one (hub.rs
// default_session_name): its program's file name and the first free
// number from the workspace's session count + 1.
function defaultSessionName(workspaceId, kind) {
  const workspace = workspaceById(workspaceId);
  const kinds = (state.kindsBy && state.kindsBy[(workspace && workspace.machine) || ""]) || state.kinds || [];
  const found = kinds.find((k) => k.kind === kind);
  const program = found && found.program ? basename(found.program) : kind;
  // As the hub numbers them: per program (`claude 1` after two shells),
  // unique on its machine.
  const machine = (workspace && workspace.machine) || "";
  const sessions = state.sessions.filter((s) => (s.machine || "") === machine);
  let n = sessions.filter((s) => s.workspace_id === workspaceId && s.name.startsWith(`${program} `)).length + 1;
  while (sessions.some((s) => s.name === `${program} ${n}`)) n++;
  return `${program} ${n}`;
}

function updateSessionPlaceholder() {
  const kind = selectedKind();
  const workspaceId = $("#session-workspace").value;
  $("#session-name").placeholder = worktrees.taskName() || defaultSessionName(workspaceId, kind);
}

// `place`: preset "Show in" ("pane", "right", "down", "tab").
async function openSessionDialog(workspaceId, place) {
  if (!state.workspaces.length) {
    openWorkspaceDialog();
    return;
  }
  const target = workspaceById(workspaceId) ||
    workspaceById(sessionById(state.attached)?.workspace_id) ||
    workspaceById(state.currentWorkspace) ||
    state.workspaces[0];
  sessionForm.reset();
  worktrees.reset();
  $("#session-error").textContent = "";
  $("#session-workspace").replaceChildren(
    ...state.workspaces.map((w) => el("option", {
      value: w.id,
      textContent: w.machine ? `${w.name} (${w.machine})` : w.name,
      selected: w.id === target.id,
    })),
  );
  launcher.reset(target.id);
  renderKinds(
    (state.kindsBy && state.kindsBy[target.machine || ""]) || state.kinds || [{ kind: "shell", available: true, program: null }],
    store.get("kind", "shell"),
  );
  renderRepoPicker();
  $("#session-place").value = place || "pane";
  placePickChanged();
  $("#dlg-session").showModal();
  const first = sessionForm.querySelector('input[name="kind"]:checked');
  if (first) first.focus();
  const kinds = await loadKinds();
  if ($("#dlg-session").open) {
    const keep = selectedKind();
    renderKinds(kinds, keep);
    updateSessionPlaceholder();
    const checked = sessionForm.querySelector('input[name="kind"]:checked');
    if (checked && sessionForm.contains(document.activeElement) === false) checked.focus();
  }
}

// "Show in" means something only for the workspace on the stage, on a
// screen with splits; another workspace's session opens in its layout.
function placePickChanged() {
  const usable = !phone() && !!state.view && $("#session-workspace").value === state.view &&
    !!activeTab() && !!focusedPane();
  $("#place-pick").hidden = !usable;
}
$("#session-workspace").addEventListener("change", () => {
  launcher.reset($("#session-workspace").value);
  renderRepoPicker();
  placePickChanged();
  // Another machine launches what is installed there.
  const keep = selectedKind();
  loadKinds().then((kinds) => {
    if ($("#dlg-session").open) renderKinds(kinds, keep);
  });
});
$("#repo-picker").addEventListener("change", pickedFolderChanged);
$("#repo-picker").addEventListener("input", (event) => {
  if (event.target.matches(".repo-filter")) pickedFolderChanged();
});
$("#wt-pick").addEventListener("input", updateSessionPlaceholder);
// Enter launches from anywhere in the form (radios do not submit on
// their own).
// In the prompt, Enter is a new line and Ctrl/Cmd Enter launches.
sessionForm.addEventListener("keydown", (event) => {
  if (event.key !== "Enter" || event.isComposing || event.target.matches("button, select")) return;
  if (event.target.matches("textarea") && !(event.ctrlKey || event.metaKey)) return;
  if (event.target.closest("#tpl-save")) return;
  event.preventDefault();
  sessionForm.requestSubmit();
});

sessionForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  // Enter submits too: the live worktree check's error stands.
  if (worktrees.blocked()) return;
  const workspaceId = $("#session-workspace").value;
  const repoInput = sessionForm.querySelector('input[name="repo"]:checked');
  const kind = selectedKind();
  const name = $("#session-name").value.trim();
  const submit = $("#session-submit");
  submit.disabled = true;
  $("#session-error").textContent = "";
  try {
    const body = { workspace_id: workspaceId, kind, ...launcher.request() };
    if (repoInput) body.repo = repoInput.value;
    const worktree = worktrees.request();
    if (worktree) body.worktree = worktree;
    if (name) body.name = name;
    if (repoInput) {
      state.repoChoice[workspaceId] = repoInput.value;
      store.set("repo-choice", state.repoChoice);
    }
    store.set("kind", kind);
    const pick = $("#place-pick").hidden ? "pane" : $("#session-place").value;
    const place = pick === "tab" ? { newTab: true } : pick === "right" || pick === "down" ? { split: pick } : {};
    $("#dlg-session").close("ok");
    await createSession(body, place);
  } catch (error) {
    if (!$("#dlg-session").open) $("#dlg-session").showModal();
    $("#session-error").textContent = error.message;
  } finally {
    submit.disabled = worktrees.blocked();
  }
});

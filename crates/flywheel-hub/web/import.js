// flywheel hub: "Import VS Code workspace" in the new-workspace dialog.
// Lists the *.code-workspace files the hub finds under a search root
// (GET /api/code-workspaces[?root=]), or takes a pasted path, and imports
// one (POST /api/workspaces/import): its folders, git repos or plain
// directories, with their names. What could not be imported (missing
// folders, remote ones) is listed, never dropped silently. app-dialogs.js
// creates it with its helpers.
"use strict";

window.FwHubImport = function (ctx) {
  const { $, el, api, refresh, rememberWorkspace, basename, workspaceById } = ctx;

  const ui = {
    root: "", // the search root last scanned (as the hub resolved it)
    scanned: false,
    busy: false,
    seq: 0, // drops answers to superseded scans
  };

  const section = $("#ws-import");
  const toggle = $("#import-toggle");
  const body = $("#import-body");
  const rootInput = $("#import-root");
  const pathInput = $("#import-path");
  const list = $("#import-files");
  const report = $("#import-report");
  const error = $("#import-error");

  const { plural } = window.FwHubFmt;

  function setOpen(open) {
    toggle.setAttribute("aria-expanded", String(open));
    body.hidden = !open;
    section.classList.toggle("open", open);
    if (open && !ui.scanned) scan();
  }

  // Back to the closed panel (the dialog opened again); hidden entirely
  // when editing an existing workspace.
  function reset(creating) {
    section.hidden = !creating;
    report.hidden = true;
    report.replaceChildren();
    error.textContent = "";
    pathInput.value = "";
    $("#import-update").checked = false;
    list.hidden = false;
    setOpen(false);
  }

  async function scan() {
    const seq = ++ui.seq;
    const root = rootInput.value.trim();
    error.textContent = "";
    list.replaceChildren(el("li", { className: "import-empty", textContent: "Looking for *.code-workspace files..." }));
    try {
      const found = await api("GET", `/api/code-workspaces${root ? `?root=${encodeURIComponent(root)}` : ""}`);
      if (seq !== ui.seq) return;
      ui.scanned = true;
      ui.root = found.root;
      if (!root) rootInput.value = found.root;
      renderFiles(found);
    } catch (e) {
      if (seq !== ui.seq) return;
      list.replaceChildren();
      error.textContent = e.message;
    }
  }

  function renderFiles(found) {
    if (!found.files.length) {
      list.replaceChildren(el("li", {
        className: "import-empty",
        textContent: `No *.code-workspace files within ${found.depth} levels of ${found.root}.`,
      }));
      return;
    }
    const rows = found.files.map((file) => {
      const shortPath = file.path.startsWith(found.root + "/") ? file.path.slice(found.root.length + 1) : file.path;
      const meta = file.error
        ? el("span", { className: "import-meta bad", textContent: file.error })
        : el(
          "span",
          { className: "import-meta" },
          plural(file.folders, "folder"),
          file.skipped ? ` · ${file.skipped} not local` : "",
          file.workspace_id ? el("b", { className: "import-exists", textContent: " · workspace exists" }) : "",
        );
      const action = file.error
        ? null
        : el("button", {
          type: "button",
          className: `slab${file.workspace_id ? " ghost" : ""}`,
          textContent: file.workspace_id ? "Update" : "Import",
          title: file.workspace_id
            ? `Replace the folders of workspace ${file.name} with this file's`
            : `Create workspace ${file.name}`,
          onclick: () => (file.workspace_id ? confirmUpdate(file, row) : importFile(file.path, false)),
        });
      const row = el(
        "li",
        { className: `import-file${file.error ? " broken" : ""}` },
        el(
          "span",
          { className: "import-what" },
          el("b", { textContent: file.name || basename(file.path) }),
          el("small", { textContent: shortPath, title: file.path }),
          meta,
        ),
        action,
      );
      return row;
    });
    if (found.truncated) {
      rows.push(el("li", { className: "import-empty", textContent: "Stopped early: too many directories. Narrow the search root." }));
    }
    list.replaceChildren(...rows);
  }

  // Update replaces a workspace's folders and labels: say what changes
  // and ask first.
  function confirmUpdate(file, row) {
    list.querySelectorAll(".import-confirm").forEach((c) => c.remove());
    const ws = workspaceById(file.workspace_id);
    const now = ws ? ws.folders.map((f) => f.path) : [];
    const next = file.paths || [];
    const added = next.filter((p) => !now.includes(p));
    const removed = ws ? ws.folders.filter((f) => !next.includes(f.path)) : [];
    const names = (paths) => paths.map((p) => basename(p)).join(", ");
    const lines = [];
    if (added.length) lines.push(`adds ${names(added)}`);
    if (removed.length) lines.push(`drops ${removed.map((f) => f.name || basename(f.path)).join(", ")}`);
    if (!lines.length) lines.push("same folders");
    lines.push("labels come from the file");
    const confirm = el(
      "div",
      { className: "import-confirm", role: "group", "aria-label": `Update ${file.name}?` },
      el("span", {}, el("b", { textContent: `Replace ${file.name}'s ${plural(now.length, "folder")}?` }), ` ${lines.join(" · ")}.`),
      el("button", { type: "button", className: "slab ghost", textContent: "Keep", onclick: () => confirm.remove() }),
      el("button", {
        type: "button",
        className: "slab",
        textContent: "Replace",
        onclick: () => importFile(file.path, true),
      }),
    );
    row.append(confirm);
    confirm.querySelector(".slab:not(.ghost)").focus();
  }

  async function importFile(path, update) {
    if (ui.busy) return;
    ui.busy = true;
    error.textContent = "";
    section.querySelectorAll("button").forEach((b) => (b.disabled = true));
    try {
      const done = await api("POST", "/api/workspaces/import", { path, update });
      rememberWorkspace(done.workspace.id);
      showReport(done);
      await refresh();
    } catch (e) {
      error.textContent = e.message;
    } finally {
      ui.busy = false;
      section.querySelectorAll("button").forEach((b) => (b.disabled = false));
    }
  }

  // What the import made, every folder and every skipped entry.
  function showReport(done) {
    const ws = done.workspace;
    const plain = ws.folders.filter((f) => !f.git).length;
    list.hidden = true;
    report.hidden = false;
    report.replaceChildren(
      el(
        "p",
        { className: "import-headline" },
        el("b", { textContent: `${done.created ? "Created" : "Updated"} ${ws.name}` }),
        ` · ${plural(ws.folders.length, "folder")}${plain ? `, ${plain} without git` : ""}`,
      ),
      el(
        "ul",
        { className: "import-folders" },
        ws.folders.map((f) =>
          el(
            "li",
            { title: f.path },
            el("span", { className: `folder-tag${f.git ? " git" : ""}`, textContent: f.git ? "git" : "dir" }),
            el("span", { textContent: f.name || basename(f.path) }),
            el("small", { textContent: f.path }),
          )),
        done.skipped.map((s) =>
          el(
            "li",
            { className: "skipped", title: s.path || s.entry },
            el("span", { className: "folder-tag bad", textContent: "skip" }),
            el("span", { textContent: s.entry }),
            el("small", { textContent: s.reason }),
          )),
      ),
      el(
        "div",
        { className: "import-actions" },
        el("button", {
          type: "button",
          className: "text-btn",
          textContent: "import another",
          onclick: () => {
            report.hidden = true;
            list.hidden = false;
            scan();
          },
        }),
        el("button", {
          type: "button",
          className: "slab",
          textContent: "Done",
          onclick: () => $("#dlg-workspace").close("ok"),
        }),
      ),
    );
  }

  toggle.addEventListener("click", () => setOpen(body.hidden));
  $("#import-scan").addEventListener("click", scan);
  $("#import-path-go").addEventListener("click", () => {
    const path = pathInput.value.trim();
    if (!path) {
      error.textContent = "Paste the path of a .code-workspace file on the hub's machine.";
      pathInput.focus();
      return;
    }
    importFile(path, $("#import-update").checked);
  });
  // Enter in the panel's fields acts on them, not on the workspace form.
  rootInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.isComposing) {
      event.preventDefault();
      scan();
    }
  });
  pathInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.isComposing) {
      event.preventDefault();
      $("#import-path-go").click();
    }
  });

  return { reset, open: () => setOpen(true) };
};

// flywheel hub: Review -- a session's git work from anywhere. The branch
// against its base (commits; changed files with +/- counts and per-file
// diffs, unified or side by side), the working tree apart (conflicts,
// staged, unstaged, untracked), and guarded actions: commit all, push,
// open a pull request (gh), rebase onto the base, merge into the base in
// the main checkout, then remove the worktree. Every action is planned
// first (dry run: the exact commands, what they touch, why not) and runs
// only with that plan's fingerprint. A drawer over the terminal on
// desktop, a full-screen sheet on phones. app-base.js creates it with its
// helpers; protocol: GET /api/sessions/{id}/review, .../review/diff,
// POST .../review/{commit,push,pr,rebase,merge,ask}.
"use strict";

window.FwHubReview = function (ctx) {
  const { $, el, icon, api, state, sessionById, toast, age } = ctx;

  // Diff lines shown before a file's card is collapsed by default.
  const LARGE_LINES = 400;
  // Cards opened at once when a tab opens (desktop), by changed lines.
  const AUTO_OPEN_LINES = 600;
  const MODE_KEY = "fw-hub.review-mode";
  const DRAFT_KEY = "fw-hub.review-drafts";
  // After asking the agent for a message: how long its answer is waited
  // for, and how often its suggested blocks are checked.
  const ASK_WAIT_MS = 180000;
  const ASK_POLL_MS = 2000;

  const ui = {
    id: null, // the session whose review is open
    data: null,
    error: "",
    loading: false,
    seq: 0,
    tab: null, // "commits" | "changes" | "work"
    files: new Map(), // key -> {open, page, lines, next, loading, error, total, truncated, binary}
    badge: "", // the session's git badge when last read
    refreshTimer: null,
    mode: (() => {
      try {
        return localStorage.getItem(MODE_KEY) === "split" ? "split" : "unified";
      } catch (_) {
        return "unified";
      }
    })(),
  };

  const phone = () => window.matchMedia("(max-width: 760px)").matches;
  const wide = () => window.matchMedia("(min-width: 1000px)").matches;
  // replaceChildren, skipping the null / false a conditional child gives.
  const fill = (node, ...children) => node.replaceChildren(...children.flat().filter((c) => c !== null && c !== undefined && c !== false));
  const { plural } = window.FwHubFmt;
  // Shortcut hints follow the keymap ("" when unbound).
  const withKey = (text, id) => window.FwHubFmt.withKey(text, ctx.keyOf(id));
  const drawer = $("#review");
  const body = $("#rv-body");

  // --- data --------------------------------------------------------------

  function badgeOf(session) {
    return session && session.git ? JSON.stringify(session.git) : "";
  }

  async function load(quiet) {
    const id = ui.id;
    if (!id) return;
    const seq = ++ui.seq;
    ui.loading = true;
    if (!quiet) renderDrawer();
    try {
      const data = await api("GET", `/api/sessions/${id}/review`);
      if (seq !== ui.seq || ui.id !== id) return;
      const before = ui.data;
      ui.data = data;
      ui.error = "";
      ui.badge = badgeOf(sessionById(id));
      // A changed tree: diffs read before are stale.
      if (!before || JSON.stringify(fileKeys(before)) !== JSON.stringify(fileKeys(data)) || before.head !== data.head) {
        for (const [key, file] of ui.files) {
          if (file.open) ui.files.set(key, { open: true });
          else ui.files.delete(key);
        }
      }
      if (!ui.tab) ui.tab = defaultTab(data);
    } catch (error) {
      if (seq !== ui.seq) return;
      ui.error = error.message;
    } finally {
      if (seq === ui.seq) ui.loading = false;
    }
    renderDrawer();
  }

  function fileKeys(data) {
    const sets = ["committed", "staged", "unstaged"].map((k) =>
      (data[k].files || []).map((f) => `${k}:${f.path}:${f.insertions}:${f.deletions}`),
    );
    return [sets, (data.untracked || []).map((u) => u.path), data.conflicts];
  }

  function dirty(data) {
    return !!(data.staged.files.length || data.unstaged.files.length || data.untracked.length || data.conflicts.length);
  }

  function defaultTab(data) {
    if (data.operation || data.conflicts.length) return "work";
    if (data.committed.files.length) return "changes";
    if (dirty(data)) return "work";
    return "commits";
  }

  // --- open / close ---------------------------------------------------------

  function reviewable(session) {
    return !!session && (!!session.git || !!session.worktree);
  }

  // `follow`: re-targeted to the newly attached session (keep the focus
  // where it is).
  function open(id, follow = false) {
    const session = sessionById(id);
    if (!reviewable(session)) return;
    if (ui.id !== id) {
      ui.data = null;
      ui.tab = null;
      ui.files = new Map();
    }
    ui.id = id;
    ui.error = "";
    ctx.onOpen();
    renderDrawer();
    if (!follow) drawer.focus({ preventScroll: true });
    load();
    renderPill();
  }

  function close() {
    if (!ui.id) return;
    ui.id = null;
    ui.seq++;
    renderDrawer();
    renderPill();
    ctx.focusTerminal();
  }

  function togglePill() {
    const id = state.attached;
    if (ui.id && ui.id === id) close();
    else if (id) open(id);
  }

  $("#rv-close").addEventListener("click", close);
  $("#rv-refresh").addEventListener("click", () => load());
  drawer.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !document.querySelector("dialog[open]")) {
      event.stopPropagation();
      close();
    }
  });

  function renderPill() {
    const pill = $("#rv-toggle");
    const session = sessionById(state.attached);
    const show = reviewable(session);
    pill.hidden = !show;
    if (!show) return;
    pill.setAttribute("aria-pressed", String(ui.id === session.id));
    const git = session.git;
    const count = $("#rv-count");
    const n = git ? (git.ahead || 0) : 0;
    count.hidden = !n;
    count.textContent = n ? `↑${n}` : "";
    pill.dataset.dirty = String(!!(git && (git.dirty || git.untracked)));
    pill.title = withKey(`Review ${git && git.branch ? git.branch : "this branch"}${git && git.against ? ` against ${git.against}` : ""}`, "review.toggle");
  }

  // The session's badge moved (a commit, an edit): read the review again,
  // a moment later so a burst coalesces.
  function render() {
    renderPill();
    if (!ui.id) return;
    // The attached session changed under the drawer (a hash link, a
    // notification, the palette, a companion flip): its actions must
    // never act on a session the user has left. A phone's full-screen
    // sheet closes; the desktop drawer follows the new session.
    if (state.attached && state.attached !== ui.id) {
      if (!phone() && reviewable(sessionById(state.attached))) open(state.attached, true);
      else close();
      return;
    }
    const session = sessionById(ui.id);
    if (!session) {
      close();
      return;
    }
    const badge = badgeOf(session);
    if (badge && badge !== ui.badge && !ui.loading) {
      ui.badge = badge;
      clearTimeout(ui.refreshTimer);
      ui.refreshTimer = setTimeout(() => load(true), 400);
    }
  }

  // --- drawer ---------------------------------------------------------------

  function renderDrawer() {
    drawer.hidden = !ui.id;
    document.body.classList.toggle("rv-open", !!ui.id);
    if (!ui.id) return;
    const session = sessionById(ui.id);
    const data = ui.data;
    $("#rv-title").textContent = session ? session.name : "";
    const where = $("#rv-where");
    if (!data) {
      where.textContent = "";
      fill($("#rv-tabs"));
      fill($("#rv-actions"));
      fill(body, el("p", { className: "rv-empty", textContent: ui.error || "Reading git..." }));
      return;
    }
    const branch = data.branch || (data.rebasing ? `${data.rebasing} (rebasing)` : "detached HEAD");
    const up = data.upstream;
    fill(where,
      icon("branch"),
      el("b", { textContent: branch }),
      el("span", { textContent: data.base ? `vs ${data.base.name}` : "(no base)" }),
      data.base ? el("span", { className: "ahead", textContent: `↑${data.ahead}` }) : null,
      data.base && data.behind > 0 ? el("span", { className: "behind", textContent: `↓${data.behind}` }) : null,
      up
        ? el("span", { className: "rv-up", textContent: up.ahead || up.behind ? `· ${up.name} ↑${up.ahead} ↓${up.behind}` : `· ${up.name} in sync` })
        : data.branch
          ? el("span", { className: "rv-up", textContent: "· not pushed" })
          : null,
    );
    if (data.fanout) {
      where.append(el("span", {
        className: "rv-up",
        textContent: `· fan-out ${data.fanout.name}: ${data.fanout.candidate}${data.fanout.winner ? ` (${data.fanout.winner} won)` : ""}`,
      }));
    }
    where.title = data.dir;
    renderTabs(data);
    renderActions(data);
    const scroll = body.scrollTop;
    const peer = ctx.handoff && (data.mergeable || data.base) ? ctx.handoff().reviewPanel(session) : null;
    fill(body, peer, ...renderTab(data));
    body.scrollTop = scroll;
  }

  function renderTabs(data) {
    const work = data.staged.files.length + data.unstaged.files.length + data.untracked.length + data.conflicts.length;
    const tabs = [
      ["commits", "Commits", String(data.commits.length) + (data.ahead > data.commits.length ? "+" : "")],
      ["changes", "Changes", `${data.committed.files.length}`],
      ["work", "Working tree", `${work}`],
    ];
    const row = tabs.map(([key, label, n]) =>
      el(
        "button",
        {
          type: "button",
          role: "tab",
          className: `rv-tab${ui.tab === key ? " on" : ""}${key === "work" && data.conflicts.length ? " bad" : ""}`,
          "aria-selected": String(ui.tab === key),
          onclick: () => {
            ui.tab = key;
            renderDrawer();
            body.scrollTop = 0;
          },
        },
        label,
        el("span", { className: "rv-n", textContent: n }),
      ),
    );
    const modes = wide()
      ? el(
          "span",
          { className: "rv-modes", role: "group", "aria-label": "Diff layout" },
          ...["unified", "split"].map((mode) =>
            el("button", {
              type: "button",
              className: `rv-mode${ui.mode === mode ? " on" : ""}`,
              textContent: mode === "split" ? "side by side" : "unified",
              "aria-pressed": String(ui.mode === mode),
              onclick: () => {
                ui.mode = mode;
                try {
                  localStorage.setItem(MODE_KEY, mode);
                } catch (_) {
                  // Private mode: the choice lasts this page.
                }
                renderDrawer();
              },
            }),
          ),
        )
      : null;
    fill($("#rv-tabs"), ...row, modes);
  }

  function renderTab(data) {
    if (ui.tab === "commits") return renderCommits(data);
    if (ui.tab === "changes") {
      if (!data.base) {
        return [note("No base to compare against: this branch has no upstream and the session no worktree base.")];
      }
      const set = data.committed;
      if (!set.files.length) return [note(`Nothing committed beyond ${data.base.name} yet.`)];
      autoOpen("committed", set.files);
      return [
        summaryLine(set, `committed since ${data.base.name}`),
        ...set.files.map((f) => fileCard("committed", f)),
        set.truncated ? note("More files changed than are listed.") : null,
      ].filter(Boolean);
    }
    const out = [];
    if (data.operation) {
      out.push(
        el(
          "div",
          { className: "rv-op bad" },
          el("b", { textContent: `A ${data.operation} is in progress.` }),
          data.operation === "rebase"
            ? " Resolve the conflicts, git add them, git rebase --continue -- or ask the agent to."
            : " Finish or abort it in the terminal.",
          data.operation === "rebase" && data.agent
            ? el("button", { type: "button", className: "slab ghost rv-ask", textContent: `Ask ${data.agent.name} to resolve`, onclick: () => askAgent("resolve_conflicts") })
            : null,
          data.operation === "rebase"
            ? el("button", { type: "button", className: "slab ghost rv-ask", textContent: "Abort the rebase…", onclick: () => plan("abort") })
            : null,
        ),
      );
    }
    if (data.conflicts.length) {
      out.push(section(`Conflicts`, data.conflicts.length, data.conflicts.map((path) => fileCard("unstaged", { path, status: "unmerged", insertions: null, deletions: null, binary: false }))));
    }
    if (data.staged.files.length) {
      autoOpen("staged", data.staged.files);
      out.push(section("Staged", data.staged.files.length, data.staged.files.map((f) => fileCard("staged", f)), data.staged));
    }
    if (data.unstaged.files.length) {
      autoOpen("unstaged", data.unstaged.files);
      out.push(section("Unstaged", data.unstaged.files.length, data.unstaged.files.map((f) => fileCard("unstaged", f)), data.unstaged));
    }
    if (data.untracked.length) {
      out.push(
        section(
          "Untracked",
          data.untracked.length,
          data.untracked.map((u) => fileCard("untracked", { path: u.path, status: "untracked", insertions: null, deletions: null, binary: false })),
        ),
      );
    }
    if (!out.length) out.push(note("The working tree is clean."));
    return out;
  }

  function note(text) {
    return el("p", { className: "rv-empty", textContent: text });
  }

  function summaryLine(set, what) {
    return el(
      "p",
      { className: "rv-sum" },
      `${plural(set.files.length, "file")} ${what}: `,
      el("span", { className: "add", textContent: `+${set.insertions}` }),
      " ",
      el("span", { className: "del", textContent: `-${set.deletions}` }),
    );
  }

  function section(title, n, cards, set) {
    return el(
      "section",
      { className: "rv-sec" },
      el(
        "h3",
        {},
        title,
        el("span", { className: "rv-n", textContent: String(n) }),
        set ? el("span", { className: "rv-sec-n" }, el("span", { className: "add", textContent: `+${set.insertions}` }), " ", el("span", { className: "del", textContent: `-${set.deletions}` })) : null,
      ),
      ...cards,
    );
  }

  function renderCommits(data) {
    if (!data.base) return [note("No base to compare against: this branch has no upstream and the session no worktree base.")];
    if (!data.commits.length) {
      return [note(data.behind ? `No commits beyond ${data.base.name}; it has ${plural(data.behind, "commit")} this branch lacks.` : `No commits beyond ${data.base.name}.`)];
    }
    const list = el(
      "ol",
      { className: "rv-commits" },
      ...data.commits.map((c) =>
        el(
          "li",
          { className: "rv-commit" },
          el("code", { className: "rv-sha", textContent: c.short, title: c.sha }),
          el("span", { className: "rv-subj", textContent: c.subject }),
          el("span", { className: "rv-meta", textContent: `${c.author} · ${age(c.date)} ago` }),
        ),
      ),
    );
    const out = [
      el("p", { className: "rv-sum", textContent: `${plural(data.ahead, "commit")} on ${data.branch || "HEAD"} that ${data.base.name} lacks${data.behind ? `; ${data.base.name} has ${data.behind} new` : ""}.` }),
      list,
    ];
    if (data.ahead > data.commits.length) out.push(note(`Showing the newest ${data.commits.length}.`));
    return out;
  }

  // --- file cards and diffs ---------------------------------------------------

  const keyOf = (scope, f) => `${scope}:${f.old_path || ""}:${f.path}`;
  const changed = (f) => (f.insertions || 0) + (f.deletions || 0);
  const large = (f) => f.binary || changed(f) > LARGE_LINES;

  // Open the first small files of a list (desktop, once per key).
  function autoOpen(scope, files) {
    if (phone()) return;
    let budget = AUTO_OPEN_LINES;
    for (const f of files) {
      const key = keyOf(scope, f);
      if (ui.files.has(key)) continue;
      const lines = changed(f);
      if (!large(f) && lines <= budget && f.insertions !== null) {
        budget -= lines;
        ui.files.set(key, { open: true });
        fetchDiff(scope, f, 0);
      } else {
        ui.files.set(key, { open: false });
      }
    }
  }

  const STATUS = { added: "A", modified: "M", deleted: "D", renamed: "R", copied: "C", type: "T", unmerged: "U", untracked: "?" };

  function fileCard(scope, f) {
    const key = keyOf(scope, f);
    const file = ui.files.get(key) || { open: false };
    const slash = f.path.lastIndexOf("/");
    const head = el(
      "button",
      {
        type: "button",
        className: "rv-file-head",
        "aria-expanded": String(!!file.open),
        title: f.old_path ? `${f.old_path} → ${f.path}` : f.path,
        onclick: () => toggleFile(scope, f),
      },
      el("span", { className: `rv-st st-${f.status}`, textContent: STATUS[f.status] || "M" }),
      el(
        "span",
        { className: "rv-path" },
        f.old_path ? el("span", { className: "rv-old", textContent: `${f.old_path} → ` }) : null,
        slash >= 0 ? el("span", { className: "rv-dir", textContent: f.path.slice(0, slash + 1) }) : null,
        el("b", { textContent: f.path.slice(slash + 1) }),
      ),
      f.binary
        ? el("span", { className: "rv-counts", textContent: "binary" })
        : f.status === "untracked"
          ? el("span", { className: "rv-counts", textContent: "new" })
          : f.insertions !== null && f.insertions !== undefined
          ? el("span", { className: "rv-counts" }, el("span", { className: "add", textContent: `+${f.insertions}` }), el("span", { className: "del", textContent: `-${f.deletions}` }))
          : null,
      icon("chev"),
    );
    const card = el("article", { className: `rv-file${file.open ? " open" : ""}`, dataset: { key } }, head);
    // Open but unread (just opened, or the tree changed under it).
    if (file.open && !file.lines && !file.loading && !file.error) fetchDiff(scope, f, 0);
    if (file.open) card.append(renderDiff(scope, f, file));
    if (file.open && scope === "committed" && ctx.handoff && !f.binary) {
      card.append(el("div", { className: "rv-file-foot" }, el("button", {
        type: "button",
        className: "text-btn",
        textContent: "Send this file's diff to an agent…",
        onclick: () => ctx.handoff().sendDiff(ui.id, f.path, f.old_path),
      })));
    }
    return card;
  }

  function toggleFile(scope, f) {
    const key = keyOf(scope, f);
    const file = ui.files.get(key) || { open: false };
    file.open = !file.open;
    ui.files.set(key, file);
    renderDrawer();
  }

  async function fetchDiff(scope, f, offset) {
    const key = keyOf(scope, f);
    const id = ui.id;
    const file = ui.files.get(key) || { open: true };
    file.loading = true;
    file.error = "";
    ui.files.set(key, file);
    const params = new URLSearchParams({ scope, path: f.path, offset: String(offset) });
    if (f.old_path) params.set("old_path", f.old_path);
    try {
      const page = await api("GET", `/api/sessions/${id}/review/diff?${params}`);
      if (ui.id !== id) return;
      file.lines = offset ? (file.lines || []).concat(page.lines) : page.lines;
      file.next = page.next;
      file.total = page.total_lines;
      file.truncated = page.truncated;
      file.binary = page.binary;
    } catch (error) {
      file.error = error.message;
    } finally {
      file.loading = false;
    }
    if (ui.id === id) renderDrawer();
  }

  function renderDiff(scope, f, file) {
    const box = el("div", { className: "rv-diff" });
    if (file.error) {
      box.append(el("p", { className: "rv-empty bad", textContent: file.error }));
      return box;
    }
    if (!file.lines) {
      box.append(el("p", { className: "rv-empty", textContent: file.loading ? "Reading the diff..." : "" }));
      return box;
    }
    if (file.binary || f.binary) {
      box.append(el("p", { className: "rv-empty", textContent: "Binary file: no text diff." }));
      return box;
    }
    const hunks = parse(file.lines);
    if (!hunks.rows.length) {
      box.append(el("p", { className: "rv-empty", textContent: hunks.meta.join(" ") || "No textual change (mode or rename only)." }));
      return box;
    }
    if (hunks.meta.length) box.append(el("p", { className: "rv-meta-line", textContent: hunks.meta.join(" · ") }));
    const lang = langOf(f.path);
    const split = ui.mode === "split" && wide();
    box.append(split ? splitView(hunks.rows, lang) : unifiedView(hunks.rows, lang));
    box.classList.toggle("rv-sbs", split);
    if (file.next) {
      box.append(
        el("button", {
          type: "button",
          className: "slab ghost rv-more",
          textContent: file.loading ? "Loading..." : `Load more (${file.total - file.lines.length} lines left)`,
          disabled: !!file.loading,
          onclick: () => fetchDiff(scope, f, file.next),
        }),
      );
    } else if (file.truncated) {
      box.append(el("p", { className: "rv-empty", textContent: "The diff is cut here: it is larger than the hub reads (8 MiB)." }));
    }
    return box;
  }

  // Patch lines -> meta notes and rows {kind: hunk|ctx|add|del|note, old, new, text}.
  // A conflicted file comes as git's combined diff (`diff --cc`, `@@@`
  // hunks, one marker column per side): a line any side adds is an add,
  // one the result drops a del; line numbers are the first side's and the
  // result's.
  function parse(lines) {
    const meta = [];
    const rows = [];
    let inHunk = false;
    let width = 1;
    let oldNo = 0;
    let newNo = 0;
    for (const line of lines) {
      const hunk = /^(@{2,}) -(\d+)(?:,\d+)? (?:-\d+(?:,\d+)? )*\+(\d+)(?:,\d+)? @{2,}(.*)$/.exec(line);
      if (hunk) {
        inHunk = true;
        width = hunk[1].length - 1;
        oldNo = Number(hunk[2]);
        newNo = Number(hunk[3]);
        rows.push({ kind: "hunk", text: line });
        continue;
      }
      if (line.startsWith("diff --git ") || line.startsWith("diff --cc ") || line.startsWith("diff --combined ")) {
        inHunk = false;
        if (!line.startsWith("diff --git ")) meta.push("Conflicted: both sides, as git shows an unmerged file.");
        continue;
      }
      if (!inHunk) {
        if (/^(new file mode|deleted file mode|rename from|rename to|similarity index|old mode|new mode|Binary files|\* Unmerged path)/.test(line)) meta.push(line);
        continue;
      }
      if (line.startsWith("\\")) {
        rows.push({ kind: "note", text: line.slice(2) });
        continue;
      }
      const marks = line.slice(0, width);
      const text = line.slice(width);
      if (marks.includes("-")) rows.push({ kind: "del", old: marks[0] === "-" ? oldNo++ : undefined, text });
      else if (marks.includes("+")) rows.push({ kind: "add", old: marks[0] === "+" ? undefined : oldNo++, new: newNo++, text });
      else rows.push({ kind: "ctx", old: oldNo++, new: newNo++, text });
    }
    return { meta, rows };
  }

  function lineEl(kind, oldNo, newNo, text, lang) {
    return el(
      "div",
      { className: `rv-l ${kind}` },
      el("span", { className: "ln", textContent: oldNo === undefined ? "" : String(oldNo) }),
      el("span", { className: "ln", textContent: newNo === undefined ? "" : String(newNo) }),
      el("span", { className: "sg", textContent: kind === "add" ? "+" : kind === "del" ? "-" : " " }),
      el("code", {}, ...paint(text, lang)),
    );
  }

  function unifiedView(rows, lang) {
    const out = el("div", { className: "rv-code" });
    for (const r of rows) {
      if (r.kind === "hunk") out.append(el("div", { className: "rv-l hunk", textContent: r.text }));
      else if (r.kind === "note") out.append(el("div", { className: "rv-l note", textContent: r.text }));
      else out.append(lineEl(r.kind, r.old, r.new, r.text, lang));
    }
    return out;
  }

  // Side by side: deletions beside the additions that replace them.
  function splitView(rows, lang) {
    const out = el("div", { className: "rv-code rv-split" });
    const half = (kind, no, text) =>
      kind
        ? el("div", { className: `rv-h ${kind}` }, el("span", { className: "ln", textContent: String(no) }), el("code", {}, ...paint(text, lang)))
        : el("div", { className: "rv-h blank" });
    let i = 0;
    while (i < rows.length) {
      const r = rows[i];
      if (r.kind === "hunk" || r.kind === "note") {
        out.append(el("div", { className: `rv-l ${r.kind} full`, textContent: r.text }));
        i++;
        continue;
      }
      if (r.kind === "ctx") {
        out.append(el("div", { className: "rv-pair" }, half("ctx", r.old, r.text), half("ctx", r.new, r.text)));
        i++;
        continue;
      }
      const dels = [];
      const adds = [];
      while (i < rows.length && rows[i].kind === "del") dels.push(rows[i++]);
      while (i < rows.length && rows[i].kind === "add") adds.push(rows[i++]);
      for (let k = 0; k < Math.max(dels.length, adds.length); k++) {
        const d = dels[k];
        const a = adds[k];
        out.append(el("div", { className: "rv-pair" }, d ? half("del", d.old, d.text) : half(null), a ? half("add", a.new, a.text) : half(null)));
      }
    }
    return out;
  }

  // --- syntax colouring (cheap: one regex pass per line) -----------------------

  const KEYWORDS = {
    rust: "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while",
    js: "async await break case catch class const continue default delete do else export extends false finally for from function if import in instanceof let new null of return static super switch this throw true try typeof undefined var void while yield",
    py: "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None nonlocal not or pass raise return True try while with yield self",
    go: "break case chan const continue default defer else fallthrough for func go goto if import interface map nil package range return select struct switch type var true false",
    sh: "case do done elif else esac export fi for function if in local return then until while echo exit set unset readonly",
    c: "auto break case char class const continue default delete do double else enum extern false float for goto if inline int long namespace new nullptr private protected public return short signed sizeof static struct switch template this throw true try typedef union unsigned using virtual void volatile while final boolean extends implements import package interface",
    rb: "alias and begin break case class def defined do else elsif end ensure false for if in module next nil not or redo rescue retry return self super then true undef unless until when while yield",
    data: "true false null yes no",
  };
  const KW = {};
  for (const [lang, words] of Object.entries(KEYWORDS)) KW[lang] = new Set(words.split(" "));

  const EXT = {
    rs: "rust", js: "js", mjs: "js", cjs: "js", ts: "js", tsx: "js", jsx: "js", py: "py", go: "go",
    sh: "sh", bash: "sh", zsh: "sh", fish: "sh", c: "c", h: "c", cc: "c", cpp: "c", hpp: "c", java: "c",
    kt: "c", cs: "c", swift: "c", rb: "rb", json: "data", toml: "data", yaml: "data", yml: "data",
  };

  function langOf(path) {
    const name = path.split("/").pop().toLowerCase();
    if (name === "makefile" || name === "dockerfile") return "sh";
    const ext = name.includes(".") ? name.split(".").pop() : "";
    return EXT[ext] || null;
  }

  const HASH_COMMENTS = new Set(["py", "sh", "rb", "data"]);

  function tokenizer(lang) {
    const parts = [];
    if (HASH_COMMENTS.has(lang)) parts.push("(?<c>#.*$)");
    if (lang !== "py" && lang !== "sh" && lang !== "rb" && lang !== "data") parts.push("(?<c2>//.*$|/\\*.*?(?:\\*/|$))");
    parts.push('(?<s>"(?:[^"\\\\]|\\\\.)*"?)');
    parts.push(lang === "rust" ? "(?<s2>'(?:\\\\.|[^\\\\'])')" : "(?<s2>'(?:[^'\\\\]|\\\\.)*'?)");
    if (lang === "js" || lang === "go" || lang === "sh") parts.push("(?<s3>`(?:[^`\\\\]|\\\\.)*`?)");
    parts.push("(?<n>\\b\\d[\\d_]*(?:\\.\\d+)?(?:e[+-]?\\d+)?\\b|\\b0x[0-9a-fA-F_]+\\b)");
    parts.push("(?<w>[A-Za-z_][A-Za-z0-9_]*)");
    return new RegExp(parts.join("|"), "g");
  }
  const TOKENIZERS = {};

  function paint(text, lang) {
    if (!lang || text.length > 1000) return [text];
    const re = TOKENIZERS[lang] || (TOKENIZERS[lang] = tokenizer(lang));
    re.lastIndex = 0;
    const out = [];
    let at = 0;
    let m;
    while ((m = re.exec(text))) {
      if (m[0] === "") {
        re.lastIndex++;
        continue;
      }
      const g = m.groups;
      let cls = null;
      if (g.c || g.c2) cls = "tk-c";
      else if (g.s || g.s2 || g.s3) cls = "tk-s";
      else if (g.n) cls = "tk-n";
      else if (g.w && KW[lang].has(g.w)) cls = "tk-k";
      if (!cls) continue;
      if (m.index > at) out.push(text.slice(at, m.index));
      out.push(el("span", { className: cls, textContent: m[0] }));
      at = m.index + m[0].length;
    }
    if (at < text.length) out.push(text.slice(at));
    return out;
  }

  // --- actions -----------------------------------------------------------------

  function renderActions(data) {
    const worktree = data.mergeable;
    const button = (label, action, opts = {}) =>
      el("button", {
        type: "button",
        className: `slab${opts.ghost ? " ghost" : ""} rv-act`,
        textContent: label,
        title: opts.title || "",
        disabled: !!opts.disabled,
        dataset: { action },
        onclick: () => plan(action),
      });
    const pushed = data.upstream && !data.upstream.ahead && !data.upstream.behind;
    const list = [
      button("Commit all…", "commit", { disabled: !dirty(data) || !!data.operation, title: dirty(data) ? "Stage everything and commit it" : "Nothing to commit" }),
      button("Push…", "push", { ghost: true, disabled: !data.branch || pushed, title: pushed ? `In sync with ${data.upstream.name}` : "Push the branch (never forced)" }),
      button("Open PR…", "pr", { ghost: true, disabled: !data.branch, title: "gh pr create from the pushed branch" }),
    ];
    if (worktree) {
      list.push(button("Rebase onto base…", "rebase", { ghost: true, disabled: !data.behind || !!data.operation, title: data.behind ? `Replay onto ${data.base && data.base.name}` : "Already on top of the base" }));
      const fo = data.fanout;
      if (fo && !fo.winner) {
        // A fan-out candidate lands through Pick winner (it merges only
        // the commit that passed the graders), never this merge.
        list.push(el("button", {
          type: "button",
          className: "slab rv-act",
          textContent: "Pick winner on the board…",
          title: `Candidate ${fo.candidate} of the fan-out "${fo.name}": land it with Pick winner, which merges only what passed its graders`,
          dataset: { action: "fanout" },
          onclick: () => {
            close();
            if (ctx.openFanout) ctx.openFanout(fo.id);
          },
        }));
      } else {
        list.push(button("Merge into base…", "merge", { disabled: !data.ahead, title: `Fast-forward ${data.base ? data.base.name : "the base"} in the main checkout` }));
      }
    }
    if (ctx.handoff) {
      list.push(el("button", {
        type: "button",
        className: "slab ghost rv-act",
        textContent: "Diff to agent…",
        title: data.base ? `Send git diff ${data.base.name}...HEAD to another agent session (capped, editable first)` : "No base to diff against",
        disabled: !data.base || !data.committed.files.length,
        dataset: { action: "handoff" },
        onclick: () => ctx.handoff().sendDiff(ui.id),
      }));
    }
    // Phones: what can run now comes first.
    if (phone()) list.sort((x, y) => Number(x.disabled) - Number(y.disabled));
    fill($("#rv-actions"), ...list);
  }

  // The action sheet.
  const sheet = $("#dlg-review");
  const sheetBody = $("#rva-body");
  const okButton = $("#rva-ok");
  const cancelButton = $("#rva-cancel");
  const act = {
    action: null,
    plan: null,
    seq: 0,
    busy: false,
    done: false,
    timer: null,
    fields: {},
    after: null, // the button the outcome offers next
    then: null, // the action to plan after this one succeeds (rebase, then merge)
    stay: false, // `after` plans in this sheet instead of closing it
  };

  sheet.addEventListener("close", () => {
    act.action = null;
    act.seq++;
    clearTimeout(act.timer);
    stopAskWatch();
  });

  const TITLES = {
    commit: ["commit all", "Commit all changes"],
    push: ["push", "Push the branch"],
    pr: ["pull request", "Open a pull request"],
    rebase: ["rebase", "Rebase onto the base"],
    abort: ["abort rebase", "Abort the rebase"],
    merge: ["merge", "Merge into the base"],
  };

  function drafts() {
    try {
      return JSON.parse(localStorage.getItem(DRAFT_KEY) || "{}") || {};
    } catch (_) {
      return {};
    }
  }
  function saveDraft(id, text) {
    const all = drafts();
    if (text) all[id] = text;
    else delete all[id];
    try {
      localStorage.setItem(DRAFT_KEY, JSON.stringify(all));
    } catch (_) {
      // Storage full or off: the draft lives in the field only.
    }
  }

  function requestBody(action) {
    const f = act.fields;
    if (action === "commit") return { message: f.message ? f.message.value : "" };
    if (action === "push") return { remote: f.remote && f.remote.value ? f.remote.value : undefined };
    if (action === "pr") {
      return {
        title: f.title ? f.title.value : undefined,
        body: f.body ? f.body.value : undefined,
        base: f.base && f.base.value.trim() ? f.base.value.trim() : undefined,
        draft: f.draft ? f.draft.checked : false,
      };
    }
    return {};
  }

  function plan(action, then) {
    if (!ui.id || !ui.data) return;
    act.then = then || null;
    act.stay = false;
    cancelButton.hidden = false;
    cancelButton.textContent = "Cancel";
    act.action = action;
    act.plan = null;
    act.done = false;
    act.after = null;
    act.prFilled = false;
    act.fields = {};
    const [kicker, title] = TITLES[action];
    $("#rva-kicker").textContent = kicker;
    $("#rva-title").textContent = title;
    okButton.textContent = "Confirm";
    okButton.className = `slab${action === "merge" || action === "rebase" || action === "abort" ? " danger" : ""}`;
    fill(sheetBody, ...editor(action), el("div", { id: "rva-plan", className: "rva-plan" }, el("p", { className: "rv-empty", textContent: "Planning..." })));
    syncOk();
    if (!sheet.open) sheet.showModal();
    const first = sheetBody.querySelector("textarea, input");
    if (first && !phone()) first.focus();
    else okButton.focus();
    replan(0);
  }

  function editor(action) {
    const f = act.fields;
    if (action === "commit") {
      f.message = el("textarea", {
        rows: 5,
        className: "rva-text",
        placeholder: "feat: what changed and why",
        spellcheck: true,
        autocapitalize: "sentences",
        value: drafts()[ui.id] || "",
        oninput: () => {
          saveDraft(ui.id, f.message.value);
          replan(350);
        },
        onkeydown: (event) => {
          if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
            event.preventDefault();
            confirmNow();
          }
        },
      });
      const out = [el("label", { className: "field" }, el("span", { className: "field-label", textContent: "Message" }), f.message)];
      if (ui.data.agent) {
        f.askNote = el("span", { className: "rva-ask-note" });
        out.push(
          el(
            "div",
            { className: "rva-ask" },
            el("button", { type: "button", className: "text-btn", textContent: `Ask ${ui.data.agent.name} to write it`, onclick: () => askAgent("commit_message") }),
            el("button", { type: "button", className: "text-btn", textContent: "Use its last text block", title: `Fill in ${ui.data.agent.name}'s newest code block that is not a shell command`, onclick: useAgentBlock }),
            f.askNote,
          ),
        );
      }
      return out;
    }
    if (action === "pr") {
      // The plan (and its fingerprint) covers the title, body, base and
      // draft: what runs is what is shown.
      f.title = el("input", { type: "text", maxLength: 256, oninput: () => replan(400), autocomplete: "off" });
      f.body = el("textarea", { rows: 6, className: "rva-text", oninput: () => replan(400) });
      f.base = el("input", { type: "text", placeholder: "the base branch", oninput: () => replan(400), autocomplete: "off", spellcheck: false });
      f.draft = el("input", { type: "checkbox", onchange: () => replan(0) });
      return [
        el("label", { className: "field" }, el("span", { className: "field-label", textContent: "Title" }), f.title),
        el("label", { className: "field" }, el("span", { className: "field-label", textContent: "Body" }), f.body),
        el(
          "div",
          { className: "rva-row" },
          el("label", { className: "field" }, el("span", { className: "field-label", textContent: "Into" }), f.base),
          el("label", { className: "cmd-check" }, f.draft, " draft"),
        ),
      ];
    }
    if (action === "push" && ui.data.remotes && ui.data.remotes.length > 1) {
      f.remote = el(
        "select",
        { onchange: () => replan(0) },
        el("option", { value: "", textContent: "its push remote (automatic)" }),
        ...ui.data.remotes.map((r) => el("option", { value: r.name, textContent: `${r.name}  ${r.url}` })),
      );
      return [el("label", { className: "field" }, el("span", { className: "field-label", textContent: "Remote" }), f.remote)];
    }
    return [];
  }

  // A field changed: the plan shown (and its fingerprint, which covers the
  // fields) is stale at once, so Confirm never sends it; the new one is
  // fetched after `delay`.
  function replan(delay) {
    clearTimeout(act.timer);
    act.seq++;
    act.plan = null;
    syncOk();
    act.timer = setTimeout(() => {
      act.timer = null;
      act.planning = fetchPlan();
    }, delay);
  }

  // Confirm from the keyboard (Ctrl Enter) right after typing: plan the
  // fields as they are now, then confirm that plan.
  async function confirmNow() {
    if (act.timer) {
      clearTimeout(act.timer);
      act.timer = null;
      act.planning = fetchPlan();
    }
    if (act.planning) await act.planning;
    okButton.click();
  }

  async function fetchPlan() {
    const action = act.action;
    const id = ui.id;
    if (!action || !id || act.done) return;
    const seq = ++act.seq;
    act.plan = null;
    syncOk();
    let plan;
    try {
      plan = await api("POST", `/api/sessions/${id}/review/${action}`, { ...requestBody(action), dry_run: true });
    } catch (error) {
      if (seq !== act.seq) return;
      fill($("#rva-plan"), el("p", { className: "rva-refusal", textContent: error.message }));
      return;
    }
    if (seq !== act.seq || act.action !== action) return;
    act.plan = plan;
    if (action === "pr" && plan.defaults && !act.prFilled) {
      // The offered title and body, once; the user's edits are kept.
      const f = act.fields;
      act.prFilled = true;
      f.title.value = plan.defaults.title;
      f.body.value = plan.defaults.body;
      f.base.placeholder = plan.defaults.base || "the base branch";
    }
    showPlan(plan);
    syncOk();
  }

  function showPlan(plan) {
    const box = $("#rva-plan");
    const details = plan.details || [];
    const shown = details.slice(0, 40);
    const commands = plan.commands.map((c) => c.display).join("\n");
    fill(box,
      el("p", { className: "rva-summary", textContent: plan.summary }),
      shown.length
        ? el("ul", { className: "rva-details" }, ...shown.map((d) => el("li", { textContent: d })), details.length > shown.length ? el("li", { className: "more", textContent: `… and ${details.length - shown.length} more` }) : null)
        : null,
      ...(plan.warnings || []).map((w) => el("p", { className: "rva-warn", textContent: w })),
      commands
        ? el(
            "div",
            { className: "rva-cmds" },
            el("span", { className: "field-label", textContent: plan.commands.length > 1 ? "Runs, in order" : "Runs" }),
            el("pre", { className: "rva-cmd", textContent: commands }),
            el("button", { type: "button", className: "text-btn", textContent: "Copy", onclick: () => ctx.copyText(commands).then((ok) => toast(ok ? "Copied the commands." : "Copy failed.")) }),
          )
        : null,
      plan.refusal
        ? emptyMessage(plan)
          ? el("p", { className: "rva-hint", textContent: "Write a message (or ask the agent), then confirm." })
          : el("p", { className: "rva-refusal", textContent: plan.refusal })
        : null,
      plan.needs_rebase
        ? el("button", { type: "button", className: "slab ghost", textContent: "Rebase onto base first…", onclick: () => planLater("rebase") })
        : null,
    );
  }

  function planLater(action) {
    // From inside the sheet: replace its content with the new action, and
    // come back to this one once it succeeds.
    plan(action, act.action);
  }

  // The commit dialog, nothing typed yet: a hint, not an error.
  function emptyMessage(plan) {
    const f = act.fields;
    return act.action === "commit" && f.message && !f.message.value.trim() && /commit message/.test(plan.refusal || "");
  }

  function syncOk() {
    const pr = act.action === "pr";
    const titleMissing = pr && act.fields.title && !act.fields.title.value.trim();
    if (act.done) {
      okButton.disabled = false;
      return;
    }
    okButton.disabled = act.busy || !act.plan || !!act.plan.refusal || titleMissing;
  }

  okButton.addEventListener("click", async () => {
    if (act.done) {
      const after = act.after;
      if (act.stay) {
        act.stay = false;
        after();
        return;
      }
      sheet.close("done");
      if (after) after();
      return;
    }
    if (!act.plan || act.plan.refusal || act.busy) return;
    const action = act.action;
    const id = ui.id;
    act.busy = true;
    okButton.textContent = "Running...";
    syncOk();
    let report;
    try {
      report = await api("POST", `/api/sessions/${id}/review/${action}`, { ...requestBody(action), confirm: act.plan.fingerprint });
    } catch (error) {
      act.busy = false;
      okButton.textContent = "Confirm";
      if (/changed since/.test(error.message)) {
        toast("The repository changed: here is the new plan.", { tone: "bad" });
        replan(0);
      } else {
        const box = $("#rva-plan");
        box.append(el("p", { className: "rva-refusal", textContent: error.message }));
        syncOk();
      }
      return;
    }
    act.busy = false;
    act.done = true;
    if (action === "commit") saveDraft(id, "");
    showOutcome(action, report);
    load(true);
    ctx.refresh();
  });

  function showOutcome(action, report) {
    const offer = [];
    act.after = null;
    cancelButton.hidden = true;
    okButton.textContent = "Done";
    okButton.className = "slab";
    if (report.ok && act.then) {
      const next = act.then;
      okButton.textContent = `${TITLES[next][1]}…`;
      // Planned in the same sheet (closing it would cancel the new plan).
      act.after = () => plan(next);
      act.stay = true;
    }
    if (action === "merge" && report.worktree) {
      const wt = report.worktree;
      const blocked = wt.blocking && wt.blocking.length;
      const text = blocked
        ? `The worktree stays: ${wt.blocking.join(", ")} still ${wt.blocking.length === 1 ? "uses" : "use"} it.`
        : wt.refusal
          ? `The worktree has more: ${wt.refusal.replace(/; commit and merge.*$/, ".")}`
          : `Nothing in ${wt.branch} is lost by removing the worktree now (the merged branch goes too).`;
      offer.push(el("p", { className: "rva-summary", textContent: text }));
      if (!blocked) {
        okButton.textContent = "Remove worktree…";
        const target = ui.id;
        const owner = sessionById(target);
        const agent = owner && owner.companion_of ? owner.companion_of : target;
        act.after = () => ctx.closeSession(agent);
        offer.push(el("button", { type: "button", className: "slab ghost", textContent: "Keep it", onclick: () => sheet.close("done") }));
      }
    }
    if (action === "rebase" && !report.ok && ui.data && ui.data.agent) {
      okButton.textContent = `Ask ${ui.data.agent.name} to resolve…`;
      act.after = () => askAgent("resolve_conflicts");
    }
    // Done is the only way out, unless the outcome offers a next step
    // (then Close declines it; a merge's "Keep it" already does).
    const declined = offer.some((n) => n.tagName === "BUTTON");
    if (act.after && !declined) {
      cancelButton.hidden = false;
      cancelButton.textContent = "Close";
    }
    if (action === "pr" && report.url) {
      offer.push(el("a", { href: report.url, target: "_blank", rel: "noopener noreferrer", className: "rva-url", textContent: report.url }));
    }
    fill(sheetBody, 
      el("p", { className: `rva-outcome${report.ok ? " ok" : " bad"}`, textContent: report.summary }),
      report.conflicts && report.conflicts.length
        ? el("ul", { className: "rva-details" }, ...report.conflicts.map((c) => el("li", { textContent: `U ${c}` })))
        : null,
      ...offer,
      ...report.ran.map((r) =>
        el(
          "details",
          { className: "rva-ran", open: !report.ok },
          el("summary", { textContent: `${r.code === 0 ? "ok" : `exit ${r.code === null ? "killed" : r.code}`}  ${r.display}` }),
          r.output ? el("pre", { className: "rva-cmd", textContent: r.output }) : null,
        ),
      ),
    );
    syncOk();
    okButton.focus();
  }

  // --- asking the agent -----------------------------------------------------------

  const watch = { timer: null, until: 0, since: 0, agent: null };

  function stopAskWatch() {
    clearTimeout(watch.timer);
    watch.timer = null;
  }

  // The agent's newest code block that is not a shell command (a commit
  // message is prose: a ```sh block is never one).
  async function newestBlock(agentId) {
    const list = await api("GET", `/api/sessions/${agentId}/commands?all=true&limit=20`);
    return (list.commands || []).find((c) => !c.shell) || null;
  }

  async function askAgent(kind) {
    const id = ui.id;
    if (!id) return;
    let plan;
    try {
      plan = await api("POST", `/api/sessions/${id}/review/ask`, { kind, dry_run: true });
    } catch (error) {
      toast(error.message, { tone: "bad" });
      return;
    }
    const ok = await ctx.confirmDialog({
      kicker: "ask the agent",
      title: `Paste into ${plan.agent.name}?`,
      body: `This prompt is pasted into ${plan.agent.name}, then Enter:\n\n${plan.prompt}`,
      ok: "Paste and send",
    });
    if (!ok) return;
    let before = null;
    try {
      before = await newestBlock(plan.agent.id);
      await api("POST", `/api/sessions/${id}/review/ask`, { kind });
    } catch (error) {
      toast(error.message, { tone: "bad" });
      return;
    }
    if (kind === "commit_message") {
      // Fill the message in once the agent answers with a new block.
      watch.agent = plan.agent.id;
      watch.since = before ? before.seq : -1;
      watch.until = Date.now() + ASK_WAIT_MS;
      if (act.fields.askNote) act.fields.askNote.textContent = `Waiting for ${plan.agent.name}…`;
      pollAgent();
    } else {
      toast(`Asked ${plan.agent.name} to resolve the conflicts.`);
    }
  }

  async function pollAgent() {
    stopAskWatch();
    if (act.action !== "commit" || Date.now() > watch.until) {
      if (act.fields.askNote) act.fields.askNote.textContent = "";
      return;
    }
    try {
      const block = await newestBlock(watch.agent);
      if (block && block.seq > watch.since) {
        fillMessage(block.text);
        return;
      }
    } catch (_) {
      // Retried on the next tick.
    }
    watch.timer = setTimeout(pollAgent, ASK_POLL_MS);
  }

  function fillMessage(text) {
    const field = act.fields.message;
    if (!field) return;
    field.value = text.replace(/\s+$/, "");
    saveDraft(ui.id, field.value);
    if (act.fields.askNote) act.fields.askNote.textContent = "Filled in from the agent's answer: read it, then confirm.";
    replan(0);
  }

  async function useAgentBlock() {
    const agent = ui.data && ui.data.agent;
    if (!agent) return;
    try {
      const block = await newestBlock(agent.id);
      if (block) fillMessage(block.text);
      else toast(`${agent.name} has written no text block yet (shell commands are not messages).`);
    } catch (error) {
      toast(error.message, { tone: "bad" });
    }
  }

  // --- palette ---------------------------------------------------------------------

  function paletteItems(cmd) {
    const session = sessionById(state.attached);
    if (!reviewable(session)) return;
    cmd(ui.id ? "Close review" : `Review ${session.name}`, [ctx.keyOf("review.toggle"), "commits, diffs, commit / push / PR / merge"].filter(Boolean).join(": "), togglePill, "branch");
  }

  window.addEventListener("resize", () => {
    if (ui.id && ui.data) renderDrawer();
  });

  return {
    open,
    close,
    togglePill,
    render,
    renderPill,
    paletteItems,
    isOpen: () => !!ui.id,
  };
};

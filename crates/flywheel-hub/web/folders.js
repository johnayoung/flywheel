// flywheel hub: the workspace dialog's folder picker, over the hub
// machine's directories (a browser's own picker cannot name a path there):
//
// * completion in each folder row: the typed parent's subdirectories
//   (GET /api/fs/dirs), Tab / Right completing like a shell;
// * the folder browser (#dlg-folders): breadcrumbs, a filter, keyboard
//   moves, marking several folders to add at once;
// * suggestions: folders other workspaces and sessions use, then repos a
//   bounded scan found (GET /api/fs/suggestions), one tap to add.
//
// app-dialogs.js creates it with its helpers and the row operations.
"use strict";

window.FwHubFolders = function (ctx) {
  const { $, el, icon, api, basename } = ctx;

  // The hub's home directory (paths under it show as ~/...), learned from
  // the first answer; per machine, like the listings.
  let home = null;
  // Listings by "<parent>" (or "<parent>\n<prefix>" for a narrowed one);
  // cleared when the dialog opens or its machine changes.
  let listings = new Map();

  function reset() {
    listings = new Map();
    home = null;
    closeSuggest();
    suggest.loaded = false;
  }

  // --- paths -------------------------------------------------------------

  // Text for a right-to-left box that ellipsizes a path's start (its end is
  // what matters): marked left-to-right, so "~/" stays in front.
  const ltr = (text) => `\u200e${text}\u200e`;

  const trimSlash = (path) => (path.length > 1 ? path.replace(/\/+$/, "") : path);

  function tilde(path) {
    if (!home || !path) return path;
    if (path === home) return "~";
    return path.startsWith(`${home}/`) ? `~${path.slice(home.length)}` : path;
  }

  function expand(raw) {
    const text = raw.trim();
    if (home && (text === "~" || text.startsWith("~/"))) return trimSlash(home + text.slice(1));
    return trimSlash(text);
  }

  // One directory's listing (prefix: only names starting with it, asked
  // for when the full listing was cut short).
  function listing(path, prefix) {
    const key = prefix ? `${path}\n${prefix.toLowerCase()}` : path;
    let found = listings.get(key);
    if (!found) {
      const query = `path=${encodeURIComponent(path)}${prefix ? `&prefix=${encodeURIComponent(prefix)}` : ""}`;
      found = api("GET", `/api/fs/dirs?${query}`).then((answer) => {
        if (answer.home) home = answer.home;
        return answer;
      });
      // A failure is not remembered: the folder may appear (or open) later.
      found.catch(() => listings.delete(key));
      listings.set(key, found);
    }
    return found;
  }

  // `name` with the part matching `query` (case-insensitive) set off.
  function marked(name, query) {
    const at = query ? name.toLowerCase().indexOf(query.toLowerCase()) : -1;
    if (at < 0) return [name];
    return [
      name.slice(0, at),
      el("mark", { textContent: name.slice(at, at + query.length) }),
      name.slice(at + query.length),
    ].filter((part) => part !== "");
  }

  function tags(entry) {
    return [
      entry.git ? el("span", { className: "folder-tag git", textContent: "git" }) : null,
      entry.link ? el("span", { className: "folder-tag", textContent: "link", title: "symbolic link" }) : null,
      entry.error ? el("span", { className: "folder-tag bad", textContent: entry.error }) : null,
    ];
  }

  // --- completion --------------------------------------------------------

  const pop = el("div", { className: "path-suggest", id: "path-suggest", hidden: true });
  const popHead = el("div", { className: "ps-head" });
  const popList = el("ul", { className: "ps-list", id: "path-suggest-list", role: "listbox", "aria-label": "Matching folders" });
  const popFoot = el(
    "div",
    { className: "ps-foot", "aria-hidden": "true" },
    el("kbd", { textContent: "Tab" }), " complete ",
    el("kbd", { innerHTML: "&uarr;" }), el("kbd", { innerHTML: "&darr;" }), " pick ",
    el("kbd", { textContent: "Enter" }), " take",
  );
  pop.append(popHead, popList, popFoot);
  // In the dialog: the top layer it is drawn in is the dialog's.
  $("#dlg-workspace").append(pop);

  const comp = {
    input: null, // the row input it is for
    parent: "", // the typed text up to its last "/"
    segment: "", // the typed text after it
    matches: [],
    active: -1,
    seq: 0,
    timer: 0,
  };

  function closeSuggest() {
    clearTimeout(comp.timer);
    comp.seq += 1;
    comp.matches = [];
    comp.active = -1;
    pop.hidden = true;
    if (comp.input) {
      comp.input.setAttribute("aria-expanded", "false");
      comp.input.removeAttribute("aria-activedescendant");
    }
  }

  // What the typed text completes: its parent directory and the segment
  // being typed, or null for text that names no directory to list.
  function splitTyped(raw) {
    const text = raw.replace(/^\s+/, "");
    if (!text || /[\r\n]/.test(text)) return null;
    if (text === "~") return { parent: "~/", shown: "~", segment: "" };
    if (!text.startsWith("/") && !text.startsWith("~/")) return null;
    const cut = text.lastIndexOf("/") + 1;
    return { parent: text.slice(0, cut), shown: text.slice(0, cut), segment: text.slice(cut) };
  }

  function scheduleSuggest(input, delay) {
    clearTimeout(comp.timer);
    comp.timer = setTimeout(() => refreshSuggest(input), delay);
  }

  async function refreshSuggest(input) {
    if (document.activeElement !== input) return;
    const typed = splitTyped(input.value);
    if (!typed) {
      closeSuggest();
      return;
    }
    const seq = ++comp.seq;
    let answer;
    try {
      answer = await listing(typed.parent);
      if (answer.truncated && typed.segment) answer = await listing(typed.parent, typed.segment);
    } catch (_) {
      // The row's own check says what is wrong with the path.
      if (seq === comp.seq) closeSuggest();
      return;
    }
    if (seq !== comp.seq || document.activeElement !== input) return;
    const { segment } = typed;
    const shown = (entries) => entries.filter((e) => !e.hidden || segment.startsWith("."));
    let matches = shown(answer.entries).filter((e) => e.name.startsWith(segment));
    if (!matches.length) {
      const lower = segment.toLowerCase();
      matches = shown(answer.entries).filter((e) => e.name.toLowerCase().startsWith(lower));
    }
    comp.input = input;
    comp.parent = typed.parent;
    comp.segment = segment;
    comp.matches = matches;
    comp.active = -1;
    if (!matches.length) {
      closeSuggest();
      return;
    }
    renderSuggest(answer);
  }

  const RENDERED = 80;

  function renderSuggest(answer) {
    const { matches, segment } = comp;
    popHead.replaceChildren(
      el("span", { className: "ps-where", textContent: ltr(tilde(answer.path)) }),
      el("span", { className: "ps-count", textContent: matches.length === 1 ? "1 folder" : `${matches.length} folders` }),
    );
    popList.replaceChildren(
      ...matches.slice(0, RENDERED).map((entry, i) =>
        el(
          "li",
          {
            id: `ps-opt-${i}`,
            className: `ps-opt${entry.git ? " git" : ""}${entry.error ? " broken" : ""}`,
            role: "option",
            "aria-selected": "false",
            title: entry.error ? `${entry.path} (${entry.error})` : entry.path,
            onpointerdown: (event) => {
              // Keep focus (and a phone's keyboard) in the input.
              event.preventDefault();
              take(i);
            },
          },
          icon("folder"),
          el(
            "span",
            { className: "ps-name" },
            el("b", { textContent: entry.name.slice(0, segment.length) }),
            entry.name.slice(segment.length),
          ),
          ...tags(entry),
        )),
    );
    if (matches.length > RENDERED) {
      popList.append(el("li", { className: "ps-more", role: "presentation", textContent: `${matches.length - RENDERED} more: keep typing` }));
    }
    comp.input.setAttribute("aria-expanded", "true");
    comp.input.setAttribute("aria-controls", popList.id);
    pop.hidden = false;
    place();
  }

  // Under the input, or above it when the screen (a phone's, over its
  // keyboard) has no room below.
  function place() {
    if (pop.hidden || !comp.input) return;
    const rect = comp.input.getBoundingClientRect();
    const view = window.visualViewport;
    const height = view ? view.height + view.offsetTop : window.innerHeight;
    const width = Math.min(Math.max(rect.width, 280), window.innerWidth - 16);
    const left = Math.max(8, Math.min(rect.left, window.innerWidth - width - 8));
    const below = height - rect.bottom - 8;
    const above = rect.top - 8;
    const up = below < 200 && above > below;
    pop.style.left = `${left}px`;
    pop.style.width = `${width}px`;
    pop.style.maxHeight = `${Math.max(120, Math.min(340, up ? above : below) - 4)}px`;
    if (up) {
      pop.style.top = "auto";
      pop.style.bottom = `${window.innerHeight - rect.top + 4}px`;
    } else {
      pop.style.bottom = "auto";
      pop.style.top = `${rect.bottom + 4}px`;
    }
    pop.classList.toggle("up", up);
  }
  window.addEventListener("resize", place);
  if (window.visualViewport) window.visualViewport.addEventListener("resize", place);
  $("#dlg-workspace").addEventListener("scroll", place, true);
  $("#dlg-workspace").addEventListener("close", closeSuggest);

  function setActive(i) {
    comp.active = i;
    [...popList.children].forEach((li, n) => {
      if (li.getAttribute("role") !== "option") return;
      const on = n === i;
      li.classList.toggle("active", on);
      li.setAttribute("aria-selected", String(on));
      if (on) li.scrollIntoView({ block: "nearest" });
    });
    if (i >= 0) comp.input.setAttribute("aria-activedescendant", `ps-opt-${i}`);
    else comp.input.removeAttribute("aria-activedescendant");
  }

  // Put `text` in the input as if typed (the row re-checks it, and the
  // completion lists what is under it).
  function setTyped(text) {
    const input = comp.input;
    input.value = text;
    input.setSelectionRange(text.length, text.length);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  }

  // Take a match: its path, ready for the next level.
  function take(i) {
    const entry = comp.matches[i];
    if (!entry) return;
    setTyped(`${comp.parent}${entry.name}/`);
  }

  // Tab / Right: the highlighted match, else the longest completion all
  // matches share (a lone match taken whole); a second Tab with nothing
  // left to complete highlights the first.
  function complete() {
    const { matches, segment } = comp;
    if (!matches.length) return false;
    if (comp.active >= 0) {
      take(comp.active);
      return true;
    }
    if (matches.length === 1) {
      take(0);
      return true;
    }
    let common = matches[0].name;
    for (const { name } of matches) {
      let n = 0;
      while (n < common.length && n < name.length && common[n].toLowerCase() === name[n].toLowerCase()) n++;
      common = common.slice(0, n);
    }
    if (common.length > segment.length) {
      setTyped(comp.parent + common);
      return true;
    }
    setActive(0);
    return true;
  }

  function onKey(event) {
    const input = event.target;
    const open = !pop.hidden && comp.input === input;
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp": {
        if (!open) {
          if (event.key === "ArrowDown") {
            event.preventDefault();
            refreshSuggest(input);
          }
          return;
        }
        event.preventDefault();
        const n = Math.min(comp.matches.length, RENDERED);
        const step = event.key === "ArrowDown" ? 1 : -1;
        setActive(comp.active < 0 ? (step > 0 ? 0 : n - 1) : (comp.active + step + n) % n);
        return;
      }
      case "Tab":
        if (open && !event.shiftKey && !event.altKey && !event.ctrlKey && !event.metaKey && complete()) {
          event.preventDefault();
        }
        return;
      case "ArrowRight":
        if (open && input.selectionStart === input.value.length && input.selectionEnd === input.value.length && complete()) {
          event.preventDefault();
        }
        return;
      case "Enter":
        if (open && comp.active >= 0 && !event.isComposing) {
          event.preventDefault();
          take(comp.active);
        } else if (open) {
          closeSuggest();
        }
        return;
      case "Escape":
        if (open) {
          // Closes the list, not the dialog.
          event.preventDefault();
          event.stopPropagation();
          closeSuggest();
        }
        return;
      default:
    }
  }

  // Wire a folder row's path input.
  function attach(input) {
    input.setAttribute("role", "combobox");
    input.setAttribute("aria-autocomplete", "list");
    input.setAttribute("aria-expanded", "false");
    input.addEventListener("input", () => scheduleSuggest(input, 90));
    input.addEventListener("keydown", onKey);
    input.addEventListener("blur", () => {
      if (comp.input === input) closeSuggest();
    });
  }

  // --- the folder browser ----------------------------------------------

  const fb = {
    dialog: $("#dlg-folders"),
    filter: $("#fb-filter"),
    list: $("#fb-list"),
    error: $("#fb-error"),
    submit: $("#fb-submit"),
    row: null, // the folder row it was opened from, or null
    dir: null, // the listing on screen
    shown: [], // its entries the filter and hidden toggle leave
    active: -1,
    marked: new Map(), // path -> entry, in marking order
    seq: 0,
  };

  // Where the browser opens: the row's folder when it checked out, else
  // the parent of the last folder that did, else home.
  function startFor(row) {
    if (row && row.result && row.result.ok && !row.result.missing) return row.result.path;
    const rows = ctx.rows().filter((r) => r !== row && r.result && r.result.ok && !r.result.missing);
    const last = rows[rows.length - 1];
    if (last) return last.result.path.replace(/\/[^/]+\/?$/, "") || "/";
    return "";
  }

  async function browse(row) {
    fb.row = row || null;
    fb.marked = new Map();
    fb.error.textContent = "";
    fb.filter.value = "";
    $("#folders-kicker").textContent = ctx.machine() ? `folders on ${ctx.machine()}` : "folders on the hub's machine";
    // Last in the document: the topmost modal (see confirmDialog).
    if (document.body.lastElementChild !== fb.dialog) document.body.append(fb.dialog);
    fb.dialog.showModal();
    // A touch screen's keyboard would cover the list: the filter waits
    // for a tap.
    (window.matchMedia("(pointer: coarse)").matches ? fb.list : fb.filter).focus();
    const start = startFor(fb.row);
    if (!(await go(start)) && start) await go("");
  }

  // Show `path`'s subdirectories; false (the error shown) when it cannot.
  async function go(path, focusName) {
    const seq = ++fb.seq;
    fb.list.setAttribute("aria-busy", "true");
    try {
      const answer = await listing(path);
      if (seq !== fb.seq) return true;
      fb.dir = answer;
      fb.error.textContent = "";
      fb.filter.value = "";
      render(focusName);
      return true;
    } catch (error) {
      if (seq !== fb.seq) return true;
      fb.error.textContent = error.message;
      if (!fb.dir) renderEmpty("Nothing to show.");
      return false;
    } finally {
      if (seq === fb.seq) fb.list.removeAttribute("aria-busy");
    }
  }

  function crumbs(path) {
    const out = [];
    let base = "";
    let rest = path;
    if (home && (path === home || path.startsWith(`${home}/`))) {
      out.push({ label: "~", path: home });
      base = home;
      rest = path.slice(home.length);
    } else {
      out.push({ label: "/", path: "/" });
    }
    for (const part of rest.split("/").filter(Boolean)) {
      base = `${base}/${part}`;
      out.push({ label: part, path: base });
    }
    return out;
  }

  // Folders already in the dialog: never offered again (`except`: the
  // row the browser picks for, whose folder it may pick again).
  const inDialog = (except) => new Set(ctx.rows().filter((row) => row !== except).flatMap((row) => {
    const out = [];
    if (row.result && row.result.ok) out.push(row.result.path);
    const raw = row.input.value.trim();
    if (raw) out.push(expand(raw));
    return out;
  }));

  function render(focusName) {
    const dir = fb.dir;
    const trail = crumbs(dir.path);
    $("#fb-crumbs").replaceChildren(...trail.map((crumb, i) => el(
      "li",
      { className: crumb.label === "/" ? "root" : "" },
      el("button", {
        type: "button",
        className: "fb-crumb",
        textContent: crumb.label,
        title: crumb.path,
        "aria-current": i === trail.length - 1 ? "location" : null,
        onclick: () => go(crumb.path),
      }),
    )));
    $("#fb-crumbs").scrollLeft = $("#fb-crumbs").scrollWidth;
    $("#fb-up").disabled = !dir.parent;
    $("#fb-home").disabled = !home || dir.path === home;
    fb.filter.placeholder = `filter ${basename(dir.path) || "/"}`;
    renderList(focusName);
  }

  function renderEmpty(text) {
    fb.shown = [];
    fb.active = -1;
    fb.list.replaceChildren(el("li", { className: "fb-empty", role: "presentation", textContent: text }));
    updateCount();
  }

  async function renderList(focusName) {
    const dir = fb.dir;
    const query = fb.filter.value.trim();
    const lower = query.toLowerCase();
    let entries = dir.entries;
    // Cut short: the server narrows by the filter as a prefix.
    if (dir.truncated && query) {
      try {
        const narrowed = await listing(dir.path, query);
        if (fb.dir !== dir || fb.filter.value.trim() !== query) return;
        entries = narrowed.entries;
      } catch (_) {
        // The full listing's first entries still filter below.
      }
    }
    const hidden = $("#fb-show-hidden").checked || query.startsWith(".");
    const matching = entries.filter((e) => (hidden || !e.hidden) && (!lower || e.name.toLowerCase().includes(lower)));
    // Names starting with the filter first.
    if (lower) matching.sort((a, b) => Number(!a.name.toLowerCase().startsWith(lower)) - Number(!b.name.toLowerCase().startsWith(lower)));
    fb.shown = matching;
    const taken = inDialog(fb.row);
    if (!matching.length) {
      const hiddenCount = entries.filter((e) => e.hidden).length;
      renderEmpty(query
        ? `No folder here matches "${query}".`
        : hiddenCount && !hidden ? `No folders here (${hiddenCount} hidden).` : "No folders here.");
      return;
    }
    fb.list.replaceChildren(...matching.map((entry, i) => {
      const added = taken.has(entry.path);
      const checked = fb.marked.has(entry.path);
      const li = el(
        "li",
        {
          id: `fb-opt-${i}`,
          className: `fb-entry${entry.git ? " git" : ""}${entry.hidden ? " hidden-dir" : ""}${entry.error ? " broken" : ""}${added ? " added" : ""}`,
          role: "option",
          "aria-selected": String(checked),
          title: entry.path,
          onclick: (event) => {
            if (event.target.closest(".fb-open")) return;
            setFbActive(i);
            if (!event.target.closest("input")) toggleMark(i);
          },
          ondblclick: (event) => {
            if (!event.target.closest("input, .fb-open")) open(i);
          },
        },
        el("input", {
          type: "checkbox",
          className: "fb-check",
          tabIndex: -1,
          checked,
          disabled: added || !!entry.error,
          "aria-label": `Mark ${entry.name}`,
          onchange: () => toggleMark(i, true),
        }),
        icon("folder"),
        el("span", { className: "fb-name" }, ...marked(entry.name, query)),
        added ? el("span", { className: "folder-tag", textContent: "added" }) : null,
        ...tags(entry),
        el(
          "button",
          {
            type: "button",
            className: "icon fb-open",
            title: `Open ${entry.name}`,
            "aria-label": `Open ${entry.name}`,
            tabIndex: -1,
            disabled: !!entry.error,
            onclick: () => open(i),
          },
          icon("chev"),
        ),
      );
      return li;
    }));
    const keep = focusName ? matching.findIndex((e) => e.name === focusName) : -1;
    setFbActive(keep >= 0 ? keep : 0);
    updateCount();
  }

  function setFbActive(i) {
    fb.active = i;
    [...fb.list.children].forEach((li, n) => li.classList.toggle("active", n === i));
    const li = fb.list.children[i];
    if (li && li.getAttribute("role") === "option") {
      li.scrollIntoView({ block: "nearest" });
      fb.list.setAttribute("aria-activedescendant", li.id);
      fb.filter.setAttribute("aria-activedescendant", li.id);
    } else {
      fb.list.removeAttribute("aria-activedescendant");
      fb.filter.removeAttribute("aria-activedescendant");
    }
  }

  // Mark or unmark the i-th entry (`fromBox`: its checkbox already flipped).
  function toggleMark(i, fromBox) {
    const entry = fb.shown[i];
    const li = fb.list.children[i];
    if (!entry || !li) return;
    const box = li.querySelector(".fb-check");
    if (box.disabled) return;
    const on = fromBox ? box.checked : !fb.marked.has(entry.path);
    if (on) fb.marked.set(entry.path, entry);
    else fb.marked.delete(entry.path);
    box.checked = on;
    li.setAttribute("aria-selected", String(on));
    updateCount();
  }

  function open(i) {
    const entry = fb.shown[i];
    if (!entry || entry.error) return;
    go(entry.path);
  }

  function up() {
    if (fb.dir && fb.dir.parent) go(fb.dir.parent, basename(fb.dir.path));
  }

  function updateCount() {
    const n = fb.marked.size;
    const dir = fb.dir;
    const here = dir ? tilde(dir.path) : "";
    const added = dir && inDialog(fb.row).has(dir.path);
    $("#fb-count").textContent = ltr(n ? `${n} marked` : here);
    $("#fb-count").title = n ? [...fb.marked.keys()].map(tilde).join("\n") : here;
    fb.submit.textContent = n ? (n === 1 ? "Add 1 folder" : `Add ${n} folders`) : added ? "Already added" : "Select this folder";
    fb.submit.disabled = !dir || (!n && added);
  }

  $("#fb-home").addEventListener("click", () => go(home || ""));
  $("#fb-up").addEventListener("click", up);
  $("#fb-show-hidden").addEventListener("change", () => fb.dir && renderList());
  fb.filter.addEventListener("input", () => {
    fb.error.textContent = "";
    if (fb.dir) renderList();
  });

  // Keys from the filter and the list alike.
  function fbKey(event) {
    if (event.isComposing) return;
    const inFilter = event.target === fb.filter;
    const emptyFilter = !fb.filter.value;
    const n = fb.shown.length;
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp":
        if (!n) return;
        event.preventDefault();
        setFbActive(fb.active < 0 ? 0 : (fb.active + (event.key === "ArrowDown" ? 1 : -1) + n) % n);
        return;
      case "PageDown":
      case "PageUp":
        if (!n) return;
        event.preventDefault();
        setFbActive(Math.max(0, Math.min(n - 1, fb.active + (event.key === "PageDown" ? 10 : -10))));
        return;
      case "Enter":
        event.preventDefault();
        if (event.ctrlKey || event.metaKey) fb.dialog.querySelector("form").requestSubmit();
        else if (fb.active >= 0) open(fb.active);
        return;
      case "ArrowRight":
        if (inFilter && fb.filter.selectionStart !== fb.filter.value.length) return;
        if (fb.active >= 0) {
          event.preventDefault();
          open(fb.active);
        }
        return;
      case "ArrowLeft":
      case "Backspace":
        if (inFilter && !emptyFilter) return;
        if (inFilter && event.key === "ArrowLeft" && fb.filter.selectionStart !== 0) return;
        event.preventDefault();
        up();
        return;
      case " ":
        if (inFilter && !emptyFilter) return;
        event.preventDefault();
        if (fb.active >= 0) toggleMark(fb.active);
        return;
      case "Escape":
        if (!emptyFilter) {
          event.preventDefault();
          fb.filter.value = "";
          renderList();
        }
        return;
      default:
        // Typing in the list goes to the filter.
        if (!inFilter && event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
          fb.filter.focus();
        }
    }
  }
  fb.filter.addEventListener("keydown", fbKey);
  fb.list.addEventListener("keydown", fbKey);

  $("#folders-form").addEventListener("submit", (event) => {
    event.preventDefault();
    if (!fb.dir) return;
    const paths = fb.marked.size ? [...fb.marked.keys()] : [fb.dir.path];
    const row = fb.row;
    fb.dialog.close("ok");
    ctx.addFolders(paths.map(tilde), row);
  });
  fb.dialog.addEventListener("close", () => {
    fb.seq += 1;
    fb.dir = null;
    fb.list.replaceChildren();
  });

  // --- suggestions -------------------------------------------------------

  const SHOWN = 6;
  const suggest = {
    items: [],
    loaded: false,
    expanded: false,
    seq: 0,
  };

  async function loadSuggestions() {
    const seq = ++suggest.seq;
    suggest.expanded = false;
    suggest.items = [];
    $("#ws-suggest-hint").textContent = "looking for repos";
    $("#ws-suggest").classList.add("loading");
    renderSuggestions();
    try {
      const answer = await api("GET", "/api/fs/suggestions");
      if (seq !== suggest.seq) return;
      if (answer.home) home = answer.home;
      suggest.items = answer.suggestions || [];
    } catch (_) {
      if (seq !== suggest.seq) return;
      suggest.items = [];
    }
    suggest.loaded = true;
    $("#ws-suggest").classList.remove("loading");
    renderSuggestions();
  }

  function renderSuggestions() {
    const section = $("#ws-suggest");
    const taken = inDialog();
    const items = suggest.items.filter((s) => !taken.has(s.path));
    if (!items.length) {
      // While the scan runs the label says so; after, nothing to offer.
      section.hidden = suggest.loaded;
      $("#suggest-list").replaceChildren();
      $("#suggest-more").hidden = true;
      return;
    }
    section.hidden = false;
    $("#ws-suggest-hint").textContent = "on the hub's machine";
    const visible = suggest.expanded ? items : items.slice(0, SHOWN);
    $("#suggest-list").replaceChildren(...visible.map((item) => el(
      "li",
      {},
      el(
        "button",
        {
          type: "button",
          className: `suggest-chip ${item.source}${item.worktree ? " worktree" : ""}${item.git ? " git" : ""}`,
          title: `Add ${item.path}`,
          onclick: () => ctx.addFolders([tilde(item.path)], null),
        },
        icon("plus"),
        el("span", { className: "sg-text" },
          el("b", { textContent: basename(item.path) }),
          el("small", { textContent: ltr(tilde(item.path).replace(/\/[^/]+$/, "/") || "/") })),
        item.source === "used"
          ? el("span", { className: "sg-why", textContent: item.workspace })
          : item.worktree ? el("span", { className: "sg-why", textContent: "worktree" }) : null,
      ),
    )));
    const more = $("#suggest-more");
    more.hidden = items.length <= SHOWN;
    more.textContent = suggest.expanded ? "fewer" : `${items.length - SHOWN} more`;
  }

  $("#suggest-more").addEventListener("click", () => {
    suggest.expanded = !suggest.expanded;
    renderSuggestions();
  });

  return {
    attach,
    browse,
    reset,
    loadSuggestions,
    renderSuggestions,
    tilde,
    expand,
    close: closeSuggest,
  };
};

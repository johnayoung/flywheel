// flywheel hub: pane layout trees (pure data, no DOM). A workspace's
// layout is {rev, tabs:[{id, name, root}]}; a root is a pane
// {type:"pane", id, session|null} or a split {type:"split", dir:"right"|
// "down", ratio, a, b} (a: left / top), or -- only as a tab's root -- a
// board {type:"board", id, folder} of one workspace folder. The server
// validates and stores them (crates/flywheel-hub/src/layout.rs; keep the
// limits in step).
"use strict";

window.FwHubLayout = (function () {
  const MAX_TABS = 12;
  const MAX_PANES = 12;
  const MAX_TAB_NAME = 32;
  const MIN_RATIO = 0.05;

  function newId(prefix) {
    const bytes = new Uint8Array(6);
    window.crypto.getRandomValues(bytes);
    return prefix + Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  }

  const clone = (value) => JSON.parse(JSON.stringify(value));
  const pane = (session) => ({ type: "pane", id: newId("p"), session: session || null });
  const tab = (name, session) => ({ id: newId("t"), name, root: pane(session) });

  // Every pane of a tree, left to right / top to bottom.
  function panes(node, out = []) {
    if (!node) return out;
    if (node.type === "pane") out.push(node);
    else if (node.type === "split") {
      panes(node.a, out);
      panes(node.b, out);
    }
    return out;
  }

  const findPane = (root, id) => panes(root).find((p) => p.id === id) || null;
  const paneShowing = (root, session) => panes(root).find((p) => p.session === session) || null;
  const sessionsOf = (root) => panes(root).map((p) => p.session).filter(Boolean);

  // Replace the node with id `id` (pane) by `replacement`; returns the new
  // root (the tree is edited in place where possible).
  function replaceNode(root, id, replacement) {
    if (root.type !== "split") return root.id === id ? replacement : root;
    root.a = replaceNode(root.a, id, replacement);
    root.b = replaceNode(root.b, id, replacement);
    return root;
  }

  // Split pane `id`: `side` is where the new pane goes ("right", "left",
  // "down", "up"). Returns {root, pane} (the new pane), or null when the
  // pane is missing or the tab is full.
  function split(root, id, side, session, ratio = 0.5) {
    const target = findPane(root, id);
    if (!target || panes(root).length >= MAX_PANES) return null;
    const fresh = pane(session);
    const dir = side === "right" || side === "left" ? "right" : "down";
    const first = side === "left" || side === "up";
    const node = {
      type: "split",
      dir,
      ratio: first ? 1 - ratio : ratio,
      a: first ? fresh : { ...target },
      b: first ? { ...target } : fresh,
    };
    return { root: replaceNode(root, id, node), pane: fresh };
  }

  // Remove pane `id`; its sibling takes the split's place. Returns the new
  // root, or null when it was the last pane.
  function close(root, id) {
    if (root.type !== "split") return root.id === id ? null : root;
    if (root.a.type === "pane" && root.a.id === id) return root.b;
    if (root.b.type === "pane" && root.b.id === id) return root.a;
    root.a = close(root.a, id) || root.a;
    root.b = close(root.b, id) || root.b;
    return root;
  }

  // The split that directly contains pane / node `id`, with which side.
  function parentOf(root, id, parent = null, side = null) {
    if (root.type !== "split") return root.id === id ? { parent, side } : null;
    return parentOf(root.a, id, root, "a") || parentOf(root.b, id, root, "b");
  }

  // Move the divider nearest pane `id` along `axis` ("right": the
  // nearest side-by-side split, "down": stacked) by `step` (ratio units;
  // positive moves it right / down, as tmux's resize-pane does). Returns
  // whether it moved.
  function resizePane(root, id, axis, step) {
    // Walk up from the pane to the first split of `axis`.
    const path = [];
    (function walk(node) {
      if (node.type !== "split") return node.id === id;
      for (const side of ["a", "b"]) {
        if (walk(node[side])) {
          path.push({ node, side });
          return true;
        }
      }
      return false;
    })(root);
    const hit = path.find((p) => p.node.dir === axis);
    if (!hit) return false;
    const next = clampRatio(hit.node.ratio + step);
    if (next === hit.node.ratio) return false;
    hit.node.ratio = next;
    return true;
  }

  const clampRatio = (r) => Math.round(Math.min(1 - MIN_RATIO, Math.max(MIN_RATIO, r)) * 1000) / 1000;

  // The split at `path` ("" = root, then "a"/"b" per level).
  function splitAt(root, path) {
    let node = root;
    for (const side of path) {
      if (!node || node.type !== "split") return null;
      node = node[side];
    }
    return node && node.type === "split" ? node : null;
  }

  // Drop sessions the hub no longer has (`known`: Set of ids), as the
  // server does when one is removed: their panes close; a tab left with no
  // pane keeps one, empty.
  function prune(layout, known) {
    let changed = false;
    for (const t of layout.tabs) {
      for (const p of panes(t.root)) {
        if (p.session && !known.has(p.session)) {
          changed = true;
          const next = close(t.root, p.id);
          t.root = next || { type: "pane", id: p.id, session: null };
        }
      }
    }
    return changed;
  }

  // Each pane's box ({w, h}, px) were `root` laid out in a `w` x `h`
  // stage with `gap`-px dividers between split sides (pane id -> box).
  function boxes(root, w, h, gap = 0, out = new Map()) {
    if (!root) return out;
    if (root.type === "pane") {
      out.set(root.id, { w: Math.max(0, w), h: Math.max(0, h) });
      return out;
    }
    if (root.type !== "split") return out;
    if (root.dir === "right") {
      const inner = w - gap;
      boxes(root.a, inner * root.ratio, h, gap, out);
      boxes(root.b, inner * (1 - root.ratio), h, gap, out);
    } else {
      const inner = h - gap;
      boxes(root.a, w, inner * root.ratio, gap, out);
      boxes(root.b, w, inner * (1 - root.ratio), gap, out);
    }
    return out;
  }

  // A unique "tab N" name.
  function nextTabName(layout, base = "tab") {
    const taken = new Set(layout.tabs.map((t) => t.name.toLowerCase()));
    for (let n = layout.tabs.length + 1; ; n++) {
      const name = `${base} ${n}`;
      if (!taken.has(name)) return name;
    }
  }

  // `base` itself when no tab has that name, else "base N".
  function uniqueTabName(layout, base) {
    const taken = new Set(layout.tabs.map((t) => t.name.toLowerCase()));
    const stem = base.slice(0, MAX_TAB_NAME - 3);
    if (!taken.has(stem.toLowerCase())) return stem;
    for (let n = 2; ; n++) {
      if (!taken.has(`${stem} ${n}`.toLowerCase())) return `${stem} ${n}`;
    }
  }

  return {
    MAX_TABS, MAX_PANES, MAX_TAB_NAME, MIN_RATIO,
    newId, clone, pane, tab, uniqueTabName, panes, findPane, paneShowing, sessionsOf, split, close, parentOf,
    resizePane, clampRatio, splitAt, prune, nextTabName, boxes,
  };
})();

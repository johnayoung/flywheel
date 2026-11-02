// flywheel hub: pane layout trees (pure data, no DOM). A workspace's
// layout is {rev, tabs:[{id, name, root}]}; a root is a pane
// {type:"pane", id, session|null} or a split {type:"split", dir:"right"|
// "down", ratio, a, b} (a: left / top). The server validates and stores
// them (crates/flywheel-hub/src/layout.rs; keep the limits in step).
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
    else {
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
    if (root.type === "pane") return root.id === id ? replacement : root;
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
    if (root.type === "pane") return root.id === id ? null : root;
    if (root.a.type === "pane" && root.a.id === id) return root.b;
    if (root.b.type === "pane" && root.b.id === id) return root.a;
    root.a = close(root.a, id) || root.a;
    root.b = close(root.b, id) || root.b;
    return root;
  }

  // The split that directly contains pane / node `id`, with which side.
  function parentOf(root, id, parent = null, side = null) {
    if (root.type === "pane") return root.id === id ? { parent, side } : null;
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
      if (node.type === "pane") return node.id === id;
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

  // A unique "tab N" name.
  function nextTabName(layout, base = "tab") {
    const taken = new Set(layout.tabs.map((t) => t.name.toLowerCase()));
    for (let n = layout.tabs.length + 1; ; n++) {
      const name = `${base} ${n}`;
      if (!taken.has(name)) return name;
    }
  }

  return {
    MAX_TABS, MAX_PANES, MAX_TAB_NAME, MIN_RATIO,
    newId, clone, pane, tab, panes, findPane, paneShowing, sessionsOf, split, close, parentOf,
    resizePane, clampRatio, splitAt, prune, nextTabName,
  };
})();

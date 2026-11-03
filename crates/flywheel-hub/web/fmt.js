// Shared formats: token counts, dollar amounts, sizes, counted words, and
// shortcut hints read the same on every surface (the header's usage meter,
// the launcher, the sidebar, the Flywheel panel, the fan-out board, ...);
// paths cut from their start; and where a popover opened from a button
// goes.
"use strict";

window.FwHubFmt = (() => {
  // 950, "1.5k", "42k", "2.00M".
  function tokens(n) {
    n = n || 0;
    if (n < 1000) return String(n);
    if (n < 1e6) return `${(n / 1e3).toFixed(n < 1e4 ? 1 : 0)}k`;
    return `${(n / 1e6).toFixed(2)}M`;
  }

  // "$0", "$0.0040" (sub-cent spend stays visible), "$0.42", "$120".
  function dollars(usd) {
    if (!usd) return "$0";
    if (usd < 0.01) return `$${usd.toFixed(4)}`;
    if (usd < 100) return `$${usd.toFixed(2)}`;
    return `$${Math.round(usd)}`;
  }

  // "1 file", "2 files".
  function plural(n, word) {
    return `${n} ${word}${n === 1 ? "" : "s"}`;
  }

  // "512 B", "1.5 KiB", "64.0 MiB", "1.00 GiB".
  function bytes(n) {
    n = n || 0;
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
    if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MiB`;
    return `${(n / 1024 / 1024 / 1024).toFixed(2)} GiB`;
  }

  // "Zoom (Alt Z)", or just "Zoom" when the action is unbound (`key`: the
  // keymap's label for it, "" when unbound).
  function withKey(text, key) {
    return key ? `${text} (${key})` : text;
  }

  // A path for a `.path-tail` element (cut from its start, so the end --
  // the repo, the worktree's task -- stays): the LRMs keep its slashes in
  // place under that styling's `direction: rtl`.
  function pathTail(path) {
    return `\u200e${path || ""}\u200e`;
  }

  // Put `pop` (position: fixed) under `trigger`: `gap` below it, its left
  // edge at the trigger's (moved left as far as it takes to stay `margin`
  // inside the window), at most `width` wide and no taller than the room
  // left below.
  function placeUnder(pop, trigger, width, { margin = 8, gap = 6 } = {}) {
    const box = trigger.getBoundingClientRect();
    const w = Math.min(width, window.innerWidth - 2 * margin);
    const left = Math.max(margin, Math.min(box.left, window.innerWidth - w - margin));
    const top = box.bottom + gap;
    pop.style.width = `${w}px`;
    pop.style.left = `${Math.round(left)}px`;
    pop.style.top = `${Math.round(top)}px`;
    pop.style.maxHeight = `min(70vh, 34rem, ${Math.max(120, Math.floor(window.innerHeight - top - margin))}px)`;
  }

  return { tokens, dollars, plural, bytes, withKey, pathTail, placeUnder };
})();

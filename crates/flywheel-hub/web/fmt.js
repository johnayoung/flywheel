// Shared formats: token counts, dollar amounts, sizes, counted words, and
// shortcut hints read the same on every surface (the header's usage meter,
// the launcher, the sidebar, the Flywheel panel, the fan-out board, ...).
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

  return { tokens, dollars, plural, bytes, withKey };
})();

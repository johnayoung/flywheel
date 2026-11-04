// flywheel hub: how good the link to the hub is, and lite mode.
//
// Lite mode is for a weak link (a phone on cellular): terminal sockets
// ask the hub for at most FPS updates a second (screen diffs once output
// outruns the link; protocol: docs/hub/api.md), hidden panes get none,
// and fetches nothing on screen needs (usage rollups, the flywheel panel's
// summaries, session re-lists beyond one per RELIST_MS) wait. The setting
// (Settings > Terminal, the header's lite pill) is auto, on, or off,
// per device; auto turns lite on when the browser says the connection is
// slow (Network Information API, where there is one) or what this page
// measures is: round trips (input acknowledged, a socket's first answer)
// and transfer rates (snapshots, the page itself), with hysteresis. One
// stall is not a slow link: acks arriving together count once, round
// trips alone need MIN_RTT_SAMPLES, and a fast transfer rate outvotes them.
"use strict";

window.FwHubNet = function (ctx) {
  const FPS = 4;
  // Round trips: slow from SLOW_RTT_MS (median of the last few), fast
  // again under FAST_RTT_MS.
  const SLOW_RTT_MS = 300;
  const FAST_RTT_MS = 150;
  // Transfers of at least MIN_SAMPLE bytes: slow under SLOW_BPS bytes a
  // second, fast again over FAST_BPS.
  const MIN_SAMPLE = 24 * 1024;
  const SLOW_BPS = 64 * 1024;
  const FAST_BPS = 256 * 1024;
  const SAMPLES = 7;
  const MIN_RTT_SAMPLES = 3;
  // Round trips ending within this of the one before are one sample
  // (what a stall acknowledges at once when it ends).
  const BURST_MS = 100;
  // Deferred fetches run at most this often while lite.
  const DEFERRED_MS = 60000;
  // While lite, session lists are re-read at most this often.
  const RELIST_MS = 3000;

  // A link measured slow is assumed slow for a while after a reload.
  const REMEMBER_MS = 10 * 60 * 1000;
  const REMEMBER_KEY = "fw-hub-net-slow";

  const rtts = [];
  let rate = null; // bytes a second, the last transfer measured
  let rateAt = 0;
  // A transfer rate measured longer ago than this no longer counts.
  const RATE_FOR_MS = 5 * 60 * 1000;
  let measuredSlow = (() => {
    try {
      return Date.now() - Number(localStorage.getItem(REMEMBER_KEY) || 0) < REMEMBER_MS;
    } catch (_) {
      return false;
    }
  })();
  function remember() {
    try {
      if (measuredSlow) localStorage.setItem(REMEMBER_KEY, String(Date.now()));
      else localStorage.removeItem(REMEMBER_KEY);
    } catch (_) {
      // Private mode: measured afresh on every load.
    }
  }
  const listeners = new Set();
  const deferred = new Map(); // key -> fn, run when lite ends (or every DEFERRED_MS)
  let shown = null;

  const connection = navigator.connection || navigator.mozConnection || navigator.webkitConnection || null;
  function connectionSlow() {
    if (!connection) return false;
    if (connection.saveData) return true;
    if (["slow-2g", "2g", "3g"].includes(connection.effectiveType)) return true;
    return (connection.rtt > 0 && connection.rtt >= SLOW_RTT_MS * 1.5) || (connection.downlink > 0 && connection.downlink < 1);
  }

  const median = (list) => {
    if (!list.length) return null;
    const sorted = [...list].sort((a, b) => a - b);
    return sorted[Math.floor(sorted.length / 2)];
  };

  function remeasure() {
    const rtt = median(rtts);
    if (rate !== null && Date.now() - rateAt > RATE_FOR_MS) rate = null;
    const rateSlow = rate !== null && rate < SLOW_BPS;
    const rateFast = rate !== null && rate > FAST_BPS;
    const rttSlow = rtt !== null && rtts.length >= MIN_RTT_SAMPLES && rtt >= SLOW_RTT_MS;
    const rttFast = rtt !== null && rtts.length >= MIN_RTT_SAMPLES && rtt < FAST_RTT_MS;
    const slowNow = rateSlow || (rttSlow && !rateFast);
    const fastNow = (rttFast && !rateSlow) || (rateFast && (rtt === null || rtt < SLOW_RTT_MS));
    if (!measuredSlow && slowNow) measuredSlow = true;
    else if (measuredSlow && fastNow && !connectionSlow()) measuredSlow = false;
    if (slowNow || fastNow) remember();
    changed();
  }

  // "auto" | "on" | "off" (this device's choice; Settings > Terminal).
  function pref() {
    const value = ctx.prefs.get("terminal.lite", "auto");
    return ["auto", "on", "off"].includes(value) ? value : "auto";
  }

  const autoSlow = () => measuredSlow || connectionSlow();

  function lite() {
    const p = pref();
    return p === "on" || (p === "auto" && autoSlow());
  }

  function changed() {
    const now = lite();
    if (now === shown) return;
    shown = now;
    if (!now) flushDeferred();
    for (const fn of listeners) {
      try {
        fn(now);
      } catch (error) {
        console.error(error);
      }
    }
  }

  function flushDeferred() {
    const jobs = [...deferred.values()];
    deferred.clear();
    for (const fn of jobs) {
      try {
        fn();
      } catch (error) {
        console.error(error);
      }
    }
  }
  setInterval(() => {
    if (deferred.size && lite()) flushDeferred();
  }, DEFERRED_MS);

  // Run `fn` now, or (lite) once lite ends or the next DEFERRED_MS tick;
  // a later `fn` with the same key replaces a waiting one.
  function defer(key, fn) {
    if (!lite()) {
      fn();
      return;
    }
    deferred.set(key, fn);
  }

  // A session re-list: at once, or (lite) at most one per RELIST_MS.
  let relistAt = 0;
  let relistTimer = 0;
  function relist(fn) {
    if (!lite()) {
      fn();
      return;
    }
    if (relistTimer) return;
    const wait = Math.max(0, relistAt + RELIST_MS - Date.now());
    relistTimer = setTimeout(() => {
      relistTimer = 0;
      relistAt = Date.now();
      fn();
    }, wait);
  }

  let rttAt = -Infinity;
  function noteRtt(ms) {
    if (!(ms >= 0) || ms > 60000) return;
    const now = performance.now();
    const burst = now - rttAt < BURST_MS;
    rttAt = now;
    if (burst) return;
    rtts.push(ms);
    while (rtts.length > SAMPLES) rtts.shift();
    remeasure();
  }

  function noteTransfer(bytes, ms) {
    if (bytes < MIN_SAMPLE || !(ms > 0)) return;
    rate = (bytes * 1000) / ms;
    rateAt = Date.now();
    remeasure();
  }

  // The page itself: how long its first answer took (a round trip), and
  // how fast the larger assets came (when they came over the network).
  function measurePage() {
    try {
      const [nav] = performance.getEntriesByType("navigation");
      if (nav && nav.responseStart > nav.requestStart) noteRtt(nav.responseStart - nav.requestStart);
      for (const entry of performance.getEntriesByType("resource")) {
        const ms = entry.responseEnd - entry.responseStart;
        if (entry.transferSize >= MIN_SAMPLE && ms > 0) noteTransfer(entry.transferSize, ms);
      }
    } catch (_) {
      // No timing API: the sockets' measures still count.
    }
  }

  if (connection && connection.addEventListener) connection.addEventListener("change", () => remeasure());
  ctx.prefs.onChange((ids) => {
    if (ids.includes("terminal.lite")) changed();
  });

  // Lite on or off for this device from the header pill: the opposite of
  // what is in force (auto's measure stays for when it is set back).
  function toggle() {
    ctx.prefs.set("terminal.lite", lite() ? "off" : "on", { device: true });
    changed();
  }

  function describe() {
    const p = pref();
    const rtt = median(rtts);
    const facts = [
      rtt !== null ? `round trip ${Math.round(rtt)} ms` : "",
      rate !== null ? `${Math.round(rate / 1024)} KB/s` : "",
      connection && connection.effectiveType ? `browser says ${connection.effectiveType}` : "",
    ].filter(Boolean).join(", ");
    const state = lite() ? "on" : "off";
    return `Lite mode ${state} (${p}${p === "auto" && autoSlow() ? ": slow link" : ""})${facts ? ` -- ${facts}` : ""}`;
  }

  measurePage();
  window.addEventListener("load", () => setTimeout(measurePage, 0));
  shown = lite();

  return {
    FPS,
    lite,
    pref,
    toggle,
    describe,
    defer,
    relist,
    noteRtt,
    noteTransfer,
    // Fetches waiting for lite to end (or the next DEFERRED_MS tick).
    pending: () => deferred.size,
    // Whether auto would pick lite (what was measured, or the browser says).
    slowLink: autoSlow,
    onChange: (fn) => listeners.add(fn),
  };
};

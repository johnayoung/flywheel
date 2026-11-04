// flywheel hub: one pane = one xterm attached to one session over its own
// WebSocket (protocol: crates/flywheel-hub/src/server.rs, docs/hub/api.md),
// with its own fit, reconnect-with-backoff, banner, and link state.
// app-stage.js lays panes out (tabs, splits) and decides which one has
// focus.
//
// Weak links (protocol 2): the pane knows the output offset its screen
// reflects, so a dropped socket comes back with what it missed instead of
// a snapshot; keystrokes typed while the link is down wait, in order, and
// go once it is back (each carries an id the hub acknowledges and drops
// when it has it already) -- unless the session changed underneath (the
// hub no longer has that output: relaunched, another hub), which the
// banner then says. Typed text is never echoed locally (the program owns
// the screen): input unacknowledged for SENDING_MS shows "sending". A
// link that went silent without closing is found by a ping/pong watchdog.
// Binary messages may come deflated (z=1, one DecompressionStream per
// socket); messages are then handled strictly in order. Lite mode
// (netlink.js) asks for paced updates, and none while the pane is hidden.
//
// Sizing: a session's PTY has one size, the last one a viewer asked for.
// A pane asks (fits the terminal to its box and resizes the PTY) only
// while it "claims" the size -- it has focus, or this page is the only
// viewer of its session. Any other pane follows the PTY's size (the server
// pushes it) and is clipped to its box, so a small unfocused pane never
// shrinks a session someone else is looking at.
"use strict";

window.FwHubPanes = function (ctx) {
  const { el, state, sessionById } = ctx;
  // Unacknowledged input older than this shows "sending".
  const SENDING_MS = 500;
  // Keystrokes kept while the link is down (beyond: refused, and said).
  const QUEUE_MAX = 4096;
  const QUEUE_BYTES = 256 * 1024;
  // Input the hub refused mid-handoff goes again after this (or at the
  // reconnect the handoff ends in).
  const RETRY_INPUT_MS = 500;
  // Reconnect attempts back off to RETRY_FAST_MS for the first
  // RETRY_FAST_FOR of a drop (a cellular blip), then to 10 s.
  const RETRY_FAST_MS = 2000;
  const RETRY_FAST_FOR = 30000;
  // A link can die without closing (a cellular handover, a NAT rebind):
  // the hub is asked for a pong when input waits UNACKED_PING_MS for its
  // ack or nothing came for QUIET_PING_MS; nothing at all for PONG_WAIT_MS
  // after that and the link is treated as lost (reconnected; what was not
  // acknowledged goes again).
  const UNACKED_PING_MS = 2000;
  const QUIET_PING_MS = 20000;
  const PONG_WAIT_MS = 6000;

  // Deflated binary messages: whether this browser inflates raw deflate.
  const canInflate = (() => {
    try {
      new DecompressionStream("deflate-raw"); // eslint-disable-line no-new
      return true;
    } catch (_) {
      return false;
    }
  })();

  // One socket's inflater: messages tagged 0 (as is) or 1 (u32 length,
  // then deflate data flushed at the message's end).
  function makeInflater() {
    const stream = new DecompressionStream("deflate-raw");
    const writer = stream.writable.getWriter();
    const reader = stream.readable.getReader();
    let spare = null;
    return {
      async take(bytes) {
        if (bytes[0] === 0) return bytes.subarray(1);
        const len = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(1);
        writer.write(bytes.subarray(5)).catch(() => {});
        const out = new Uint8Array(len);
        let got = 0;
        while (got < len) {
          let piece = spare;
          spare = null;
          if (!piece) {
            const next = await reader.read();
            if (next.done) throw new Error("inflater closed");
            piece = next.value;
          }
          const n = Math.min(piece.length, len - got);
          out.set(piece.subarray(0, n), got);
          got += n;
          if (n < piece.length) spare = piece.subarray(n);
        }
        return out;
      },
      close() {
        writer.abort().catch(() => {});
        reader.cancel().catch(() => {});
      },
    };
  }

  const randomKey = () => {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
  };

  // The server's emulator answers terminal queries for every session, so
  // the browser must not: a second viewer would answer twice.
  function muteQueries(term) {
    const answered = () => true;
    for (const id of [{ final: "c" }, { prefix: ">", final: "c" }, { final: "n" },
      { prefix: "?", final: "n" }, { intermediates: "$", final: "p" },
      { prefix: "?", intermediates: "$", final: "p" }]) {
      term.parser.registerCsiHandler(id, answered);
    }
    term.parser.registerDcsHandler({ intermediates: "$", final: "q" }, answered);
    // OSC 4 is "index;spec" pairs, OSC 10-12 a spec list; "?" asks.
    const onlyQueries = (specs) => specs.length > 0 && specs.every((spec) => spec === "?");
    term.parser.registerOscHandler(4, (data) => onlyQueries(data.split(";").filter((_, i) => i % 2 === 1)));
    for (const osc of [10, 11, 12]) {
      term.parser.registerOscHandler(osc, (data) => onlyQueries(data.split(";")));
    }
  }

  function create(paneId) {
    const head = el("header", { className: "pane-head" });
    const host = el("div", { className: "pane-term" });
    const bannerText = el("span", { className: "banner-text" });
    // "sending" / "N queued": input not yet with the hub.
    const pendingBadge = el("span", { className: "pane-pending", hidden: true, role: "status" });
    const bannerActions = el("span", { className: "banner-actions" });
    const banner = el("div", { className: "banner pane-banner", hidden: true, role: "status" }, bannerText, bannerActions);
    const root = el("section", { className: "pane", dataset: { pane: paneId } }, head, el("div", { className: "pane-body" }, host, pendingBadge), banner);

    const pane = {
      id: paneId,
      el: root,
      head,
      host,
      sessionId: null,
      term: null,
      fit: null,
      socket: null,
      exited: null, // exit info of its process ({gone:true}: no terminal on this hub)
      replaying: false, // a snapshot is being replayed at the session's size
      drawn: false, // this connection's snapshot has been replayed
      search: null, // xterm's search addon (search.js jumps to hits with it)
      size: null, // {cols, rows} the PTY has, as the server last said
      following: false, // sized to the PTY, not to its box (another viewer's size)
      retry: 0,
      retryTimer: null,
      countdown: null,
      tick: null,
      link: { kind: "idle", text: "idle" },
      disposed: false,
      // Protocol 2 (the hub said so in its first message): the output
      // offset the screen reflects, in the hub's stream `epoch`.
      v2: false,
      seq: null,
      epoch: null,
      ready: false, // this socket's first message (snapshot or resume) handled
      queue: [], // [{id, data}] typed while the link was not ready, in order
      inflight: [], // [{id, data, at}] sent, not acknowledged yet
      nextId: 1,
      key: randomKey(), // names this pane's input ids to the hub
      refused: 0, // keystrokes refused (queue full) during this drop
      lite: false, // the socket's pacing, as last asked
      paused: false,
      lostAt: 0, // when the current drop began
    };

    function makeTerm() {
      const term = new window.Terminal({
        lineHeight: 1.12,
        // Font, size, cursor, scrollback: Settings (appearance, terminal).
        ...ctx.termPrefs(),
        allowProposedApi: false,
        ...ctx.termTheme(),
      });
      const fit = new window.FitAddon.FitAddon();
      term.loadAddon(fit);
      term.loadAddon(new window.WebLinksAddon.WebLinksAddon());
      const search = new window.SearchAddon.SearchAddon();
      term.loadAddon(search);
      pane.search = search;
      term.open(host);
      muteQueries(term);
      term.onData((data) => pane.input(ctx.transformInput(data)));
      term.onBinary((data) => {
        // Legacy mouse reports only: sent when the link is up, never queued.
        const bytes = new Uint8Array(data.length);
        for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i) & 0xff;
        if (pane.ready) pane.send(bytes);
      });
      term.onResize(({ cols, rows }) => {
        // Only a claiming pane's own fit resizes the PTY; following the
        // server's size must not echo it back.
        if (!pane.following && pane.claims()) pane.send(JSON.stringify({ type: "resize", cols, rows }));
        ctx.onResize(pane);
        scheduleClip();
      });
      // The cursor moving or the view scrolling only moves the clip
      // window: the box and terminal heights measured last still hold.
      term.onCursorMove(() => scheduleClip(true));
      term.onScroll(() => scheduleClip(true));
      // Typing or clicking in a pane focuses it (its xterm took the focus).
      term.textarea.addEventListener("focus", () => ctx.onFocusIn(pane));
      pane.term = term;
      pane.fit = fit;
      ctx.attachTerminal(pane);
      let pending = 0;
      const refit = () => {
        cancelAnimationFrame(pending);
        pending = requestAnimationFrame(() => pane.refit());
      };
      pane.resizeObserver = new ResizeObserver(refit);
      pane.resizeObserver.observe(host);
    }

    pane.claims = () => ctx.claims(pane);

    // A terminal taller than its box (following another viewer's size, or
    // a phone's row floor) is clipped. The clip window follows the cursor:
    // a prompt at the top of a fresh shell stays in view as well as one at
    // the bottom of a busy agent. Rows cut off are flagged above/below.
    let clipFrame = 0;
    // The box's usable height and the terminal's, measured by the last
    // clip; `null`: measure again. Measuring forces a layout, and the
    // cursor moves on nearly every write of a busy session: only a resize
    // or a size/class change (every other caller) measures anew.
    let clipGeom = null;
    // What the last clip wrote (unchanged writes are skipped, so a clip
    // leaves the layout clean for the next pane's).
    let clipShown = { el: null };
    function scheduleClip(cursorOnly) {
      if (!cursorOnly) clipGeom = null;
      if (clipFrame) return;
      clipFrame = requestAnimationFrame(() => {
        clipFrame = 0;
        placeClip();
      });
    }
    function placeClip() {
      const xt = pane.term && pane.term.element;
      if (!xt || pane.disposed) return;
      let shift = 0;
      let overflow = 0;
      if (host.classList.contains("following") || host.classList.contains("floored")) {
        if (!clipGeom) {
          const pad = parseFloat(getComputedStyle(host).paddingTop) || 0;
          clipGeom = { avail: host.clientHeight - pad, height: xt.offsetHeight };
        }
        const { avail, height } = clipGeom;
        overflow = Math.max(0, height - avail);
        if (overflow > 0 && pane.term.rows > 0) {
          const rowH = height / pane.term.rows;
          const buf = pane.term.buffer.active;
          const row = buf.cursorY + buf.baseY - buf.viewportY;
          shift = Math.min(overflow, Math.max(0, (row + 1) * rowH - avail));
        }
      } else {
        clipGeom = null;
      }
      if (clipShown.el !== xt) clipShown = { el: xt, transform: null, above: null, below: null };
      const shown = {
        el: xt,
        transform: shift > 0 ? `translateY(${-shift}px)` : "",
        above: shift > 0.5,
        below: overflow - shift > 0.5,
      };
      if (shown.transform !== clipShown.transform) xt.style.transform = shown.transform;
      if (shown.above !== clipShown.above) host.classList.toggle("clip-above", shown.above);
      if (shown.below !== clipShown.below) host.classList.toggle("clip-below", shown.below);
      clipShown = shown;
    }

    // Size the terminal: to the box while claiming (the PTY follows),
    // else to the PTY's size (clipped, bottom rows in view).
    pane.refit = () => {
      if (!pane.term || pane.replaying || pane.disposed) return;
      // A divider is being dragged: fit once on release (app-stage.js), not on
      // every pointer move -- each fit would resize the PTY, and every
      // SIGWINCH makes the shell or agent redraw.
      if (ctx.holdFit()) return;
      if (pane.claims() || !pane.size) {
        pane.following = false;
        host.classList.remove("following");
        if (host.clientWidth > 0) ctx.fitTerminal(pane);
      } else {
        pane.following = true;
        host.classList.add("following");
        host.classList.remove("floored");
        if (pane.term.cols !== pane.size.cols || pane.term.rows !== pane.size.rows) {
          pane.term.resize(pane.size.cols, pane.size.rows);
        }
        pane.following = true;
      }
      ctx.onResize(pane);
      scheduleClip();
    };

    // This pane now claims the size (it got focus): fit and tell the PTY,
    // even when the fit itself did not change the terminal's size.
    pane.claim = () => {
      if (!pane.term) return;
      pane.refit();
      if (pane.claims() && !pane.replaying && (!pane.size || pane.size.cols !== pane.term.cols || pane.size.rows !== pane.term.rows)) {
        pane.send(JSON.stringify({ type: "resize", cols: pane.term.cols, rows: pane.term.rows }));
      }
    };

    pane.send = (message) => {
      const socket = pane.socket;
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(message);
        return true;
      }
      return false;
    };

    // Typed text (or a key bar key, a compose box's paste): to the hub
    // now, or queued while the link is not ready (see the module docs).
    pane.input = (data) => {
      if (!pane.sessionId || pane.exited || !data) return;
      const item = { id: pane.nextId++, data };
      // Behind what waits (a retry pending): in order, after it.
      if (pane.ready && !pane.queue.length && pane.socket && pane.socket.readyState === WebSocket.OPEN) {
        transmit(item);
      } else if (pane.queue.length < QUEUE_MAX && queuedBytes() + data.length <= QUEUE_BYTES) {
        pane.queue.push(item);
      } else {
        pane.refused += 1;
      }
      renderPending();
      if (pane.tick) pane.tick();
    };
    // A `{"type":"input","data"}` message (key bar, prefix keys).
    pane.sendInput = (message) => {
      let parsed = null;
      try {
        parsed = typeof message === "string" ? JSON.parse(message) : null;
      } catch (_) {
        parsed = null;
      }
      if (parsed && parsed.type === "input" && typeof parsed.data === "string") pane.input(parsed.data);
      else pane.send(message);
    };

    const queuedBytes = () => pane.queue.reduce((n, item) => n + item.data.length, 0);

    function transmit(item) {
      const message = pane.v2 ? { type: "input", data: item.data, id: item.id } : { type: "input", data: item.data };
      pane.socket.send(JSON.stringify(message));
      if (pane.v2) pane.inflight.push({ ...item, at: performance.now() });
    }

    // The hub has every input up to `id`.
    function acked(id) {
      let last = null;
      while (pane.inflight.length && pane.inflight[0].id <= id) last = pane.inflight.shift();
      if (last && ctx.net) ctx.net.noteRtt(performance.now() - last.at);
      renderPending();
    }

    // The hub refused input `id`: mid-handoff (`retry`) it and what came
    // after go again shortly -- or, the socket closing for the handoff,
    // once reconnected; else it was dropped (the hub says why).
    let retryTimer = 0;
    function refused(id, retry) {
      if (!retry) {
        pane.inflight = pane.inflight.filter((item) => item.id !== id);
        renderPending();
        return;
      }
      const at = pane.inflight.findIndex((item) => item.id >= id);
      if (at < 0) return;
      const again = pane.inflight.splice(at).map(({ id, data }) => ({ id, data }));
      pane.queue = [...again, ...pane.queue];
      renderPending();
      const socket = pane.socket;
      clearTimeout(retryTimer);
      retryTimer = setTimeout(() => {
        if (pane.socket === socket && pane.ready && pane.queue.length) flushQueue();
      }, RETRY_INPUT_MS);
    }

    // Send what waited, in order (the link is back on the same output).
    function flushQueue() {
      const waiting = pane.queue;
      pane.queue = [];
      pane.refused = 0;
      for (const item of waiting) transmit(item);
      renderPending();
    }

    // What waited was typed against a screen the session no longer has.
    function dropQueue(why) {
      const n = pane.queue.length + pane.refused;
      pane.queue = [];
      pane.inflight = [];
      pane.refused = 0;
      renderPending();
      if (n) showBanner(`${n} keystroke${n === 1 ? "" : "s"} not sent: ${why}.`, [], true);
    }

    // The link went: what was sent but not acknowledged may not have
    // arrived; it goes again first (the hub drops what it has).
    function requeueInflight() {
      if (!pane.inflight.length) return;
      pane.queue = [...pane.inflight.map(({ id, data }) => ({ id, data })), ...pane.queue];
      pane.inflight = [];
    }

    let pendingTimer = 0;
    function renderPending() {
      const queued = pane.queue.length;
      const oldest = pane.inflight.length ? performance.now() - pane.inflight[0].at : 0;
      let text = "";
      if (queued) text = `${queued} queued`;
      else if (pane.inflight.length && oldest >= SENDING_MS) text = "sending…";
      pendingBadge.hidden = !text;
      if (text) {
        pendingBadge.textContent = text;
        pendingBadge.title = queued
          ? `${queued} keystroke${queued === 1 ? "" : "s"} wait for the link; they go, in order, once it is back`
          : "The hub has not acknowledged what you typed yet";
      }
      pendingBadge.classList.toggle("queued", !!queued);
      const watch = pane.inflight.length > 0 || queued > 0;
      if (watch && !pendingTimer) {
        pendingTimer = setInterval(renderPending, 200);
      } else if (!watch && pendingTimer) {
        clearInterval(pendingTimer);
        pendingTimer = 0;
      }
    }

    // Lite pacing for this socket: asked when it opens and whenever lite
    // mode or the pane's visibility changes.
    pane.syncMode = () => {
      const lite = !!(ctx.net && ctx.net.lite());
      const paused = lite && ctx.paneHidden(pane);
      if (!pane.v2 || !pane.ready || (lite === pane.lite && paused === pane.paused)) return;
      pane.lite = lite;
      pane.paused = paused;
      pane.send(JSON.stringify({ type: "mode", lite, fps: ctx.net.FPS, paused }));
    };

    pane.sendTheme = () => pane.send(ctx.themeMessage());

    function setLink(kind, text) {
      pane.link = { kind, text: text || kind };
      ctx.onLink(pane);
    }

    function showBanner(text, actions, calm) {
      banner.hidden = false;
      banner.classList.toggle("calm", !!calm);
      bannerText.textContent = text;
      bannerActions.replaceChildren(...(actions || []).filter(Boolean));
    }
    pane.showBanner = showBanner;
    pane.hideBanner = () => {
      banner.hidden = true;
    };

    function stopRetry() {
      clearTimeout(pane.retryTimer);
      clearInterval(pane.countdown);
      pane.retryTimer = null;
      pane.countdown = null;
      pane.tick = null;
    }

    function closeSocket() {
      stopRetry();
      if (pane.socket) {
        pane.socket.onclose = null;
        pane.socket.onmessage = null;
        pane.socket.close(1000);
        pane.socket = null;
      }
      if (pane.inflater) pane.inflater.close();
      pane.inflater = null;
      pane.ready = false;
      endReplay();
    }

    // Forget the link's state: another session (or none) is shown.
    function forgetLink() {
      pane.seq = null;
      pane.epoch = null;
      pane.v2 = false;
      pane.queue = [];
      pane.inflight = [];
      pane.refused = 0;
      pane.lostAt = 0;
      pane.attachedOnce = false;
      renderPending();
    }

    // xterm draws no cursor at all -- not even the inactive outline --
    // until it has had focus or a key once; a pane attached unfocused (a
    // tab switch, a reload) would show none. Its own "show the cursor"
    // step (internal, so guarded; no focus event reaches the program)
    // marks it drawn. Whether it is hidden is still the program's call.
    function initCursor() {
      const core = pane.term && pane.term._core;
      if (core && typeof core._showCursor === "function") core._showCursor();
    }

    // Stop holding the terminal at a snapshot's size (the replay finished
    // or its socket went away) and size it by the rules again.
    function endReplay() {
      if (!pane.replaying) return;
      pane.replaying = false;
      pane.refit();
    }

    function endedActions(id) {
      return [
        el("button", { type: "button", className: "slab", onclick: () => ctx.relaunch(id, pane) }, "Relaunch"),
        el("button", { type: "button", className: "slab ghost", onclick: () => ctx.closeSession(id) }, "Close"),
      ];
    }
    pane.endedActions = endedActions;

    // Show session `id` here: attach its terminal, or say why it has none.
    // `keepScreen` (no terminal): keep the last screen drawn.
    pane.show = (id, keepScreen) => {
      const session = sessionById(id);
      if (!session) return;
      if (!pane.term) makeTerm();
      closeSocket();
      pane.sessionId = id;
      pane.retry = 0;
      forgetLink();
      pane.hideBanner();
      if (session.live) {
        pane.exited = null;
        pane.size = null;
        pane.term.reset();
        connect(id);
      } else {
        pane.exited = { gone: true };
        if (!keepScreen) pane.term.reset();
        pane.term.write(`\r\n\x1b[2m[no terminal on this hub: ${ctx.status(session).label}]\x1b[0m\r\n`);
        setLink("exited", "gone");
        showBanner(ctx.goneText(session), endedActions(id), true);
      }
    };

    // Let go of the session (the pane stays, empty).
    pane.clear = () => {
      closeSocket();
      forgetLink();
      pane.sessionId = null;
      pane.exited = null;
      pane.size = null;
      if (pane.term) pane.term.reset();
      pane.hideBanner();
      setLink("idle");
    };

    // The socket's URL: protocol 2 options, lite pacing, and the offset
    // to resume from when this pane has the session's screen.
    function socketUrl(id) {
      const scheme = window.location.protocol === "https:" ? "wss" : "ws";
      const path = ctx.socketPath ? ctx.socketPath(id) : `/api/sessions/${id}/ws`;
      const params = new URLSearchParams({ proto: "2", client: pane.key });
      if (canInflate) params.set("z", "1");
      if (ctx.net && ctx.net.lite()) {
        params.set("lite", "1");
        params.set("fps", String(ctx.net.FPS));
      }
      if (pane.v2 && pane.seq !== null && pane.epoch) {
        params.set("since", String(pane.seq));
        params.set("epoch", pane.epoch);
      }
      return `${scheme}://${window.location.host}${path}${path.includes("?") ? "&" : "?"}${params}`;
    }

    // Open (or re-open) the socket: a snapshot of the session's terminal
    // (the terminal is reset for it), or -- back on the same output -- what
    // it missed, drawn on the screen it has.
    function connect(id) {
      stopRetry();
      setLink("connecting", pane.retry ? `retry ${pane.retry}` : "connecting");
      const socket = new WebSocket(socketUrl(id));
      socket.binaryType = "arraybuffer";
      pane.socket = socket;
      pane.replaying = false;
      pane.drawn = false;
      pane.ready = false;
      pane.lite = !!(ctx.net && ctx.net.lite());
      pane.paused = false;
      const term = pane.term;
      let openedAt = 0;
      // Liveness (see PONG_WAIT_MS): when anything last came, and when a
      // ping went unanswered so far.
      let heardAt = 0;
      let pingAt = 0;
      const watchdog = setInterval(() => {
        if (pane.socket !== socket) {
          clearInterval(watchdog);
          return;
        }
        if (!pane.ready || !pane.v2) return;
        const now = performance.now();
        if (pingAt) {
          if (now - pingAt >= PONG_WAIT_MS && now - heardAt >= PONG_WAIT_MS) {
            clearInterval(watchdog);
            console.warn("fw hub: no answer from the hub; reconnecting");
            socket.onclose = null;
            socket.onmessage = null;
            try {
              socket.close(4000);
            } catch (_) {
              // Closing a dead socket may throw nothing or anything.
            }
            // Not a close code (4003 is the hub's "peer revoked"): this side's verdict.
            closed({ code: "silent", reason: "no answer" });
          }
          return;
        }
        const waiting = pane.inflight.length > 0 && now - pane.inflight[0].at >= UNACKED_PING_MS;
        if (waiting || now - heardAt >= QUIET_PING_MS) {
          pingAt = now;
          pane.send(JSON.stringify({ type: "ping" }));
        }
      }, 500);
      socket.onopen = () => {
        openedAt = performance.now();
        heardAt = openedAt;
        pane.retry = 0;
        setLink("attached");
        pane.sendTheme();
        pane.hideBanner();
        ctx.onAttached(pane);
        // Typing when the hub restarted: carry on typing here.
        if (pane.refocus) {
          pane.refocus = false;
          if (!document.querySelector("dialog[open]")) term.focus();
        }
      };
      // The link is usable: what waited goes (or, the session having
      // changed underneath, is dropped and said), pacing is asked.
      const settle = (why) => {
        pane.ready = true;
        pane.lostAt = 0;
        if (why) dropQueue(why);
        else flushQueue();
        pane.attachedOnce = true;
        pane.syncMode();
      };
      // A snapshot is drawn at the session's size: replay exactly its
      // bytes at that size, then size the terminal by the rules (a
      // claiming pane resizes the session; the program redraws). A frame
      // (lite) is drawn at the size it names.
      let owed = 0;
      let owedDone = null;
      let snapshotWire = 0;
      let snapshotAt = 0;
      const replayed = () => {
        if (pane.socket !== socket) return;
        pane.replaying = false;
        pane.drawn = true;
        initCursor();
        pane.claim();
      };
      const follow = (cols, rows) => {
        pane.size = { cols, rows };
        if (!pane.replaying && (term.cols !== cols || term.rows !== rows)) {
          pane.following = true;
          term.resize(cols, rows);
          pane.following = !pane.claims();
          host.classList.toggle("following", pane.following);
        }
        ctx.onResize(pane);
        scheduleClip();
      };
      const onBinary = async (data) => {
        let bytes = new Uint8Array(data);
        const wire = bytes.length;
        if (pane.inflater) bytes = await pane.inflater.take(bytes);
        if (pane.socket !== socket) return;
        if (owed > 0) {
          owed -= bytes.length;
          if (owedDone === replayed) snapshotWire += wire;
          const done = owed <= 0 ? owedDone : null;
          if (done === replayed && ctx.net) ctx.net.noteTransfer(snapshotWire, performance.now() - snapshotAt);
          term.write(bytes, done || undefined);
        } else {
          if (pane.seq !== null) pane.seq += bytes.length;
          term.write(bytes);
        }
      };
      const onText = (raw) => {
        let message;
        try {
          message = JSON.parse(raw);
        } catch (_) {
          return;
        }
        if (message.type === "snapshot" || message.type === "resume") {
          // The first message says which protocol the hub speaks.
          if (!pane.ready && openedAt && ctx.net) ctx.net.noteRtt(performance.now() - openedAt);
          const v2 = message.type === "resume" || typeof message.seq === "number";
          if (v2 && canInflate && !pane.inflater) pane.inflater = makeInflater();
        }
        if (message.type === "snapshot") {
          const v2 = typeof message.seq === "number";
          const wasReady = pane.ready;
          // The stream this pane last had (kept across a drop even when
          // its offset was not: a snapshot cut short).
          const before = pane.epoch;
          term.reset();
          pane.v2 = v2;
          pane.seq = v2 ? message.seq : null;
          pane.epoch = v2 ? message.epoch : null;
          pane.replaying = true;
          pane.size = { cols: message.cols, rows: message.rows };
          pane.following = true;
          term.resize(message.cols, message.rows);
          pane.following = false;
          owed = message.bytes;
          owedDone = replayed;
          snapshotWire = 0;
          snapshotAt = performance.now();
          if (!(owed > 0)) replayed();
          // (A snapshot mid-socket: a v1 hub's catch-up after `reset`.)
          if (!wasReady) {
            const changed = v2 && (message.continued === false || (before && message.epoch !== before));
            settle(
              changed
                ? "the session changed while the link was down (relaunched, or another hub)"
                : !v2 && pane.attachedOnce
                  ? "this hub cannot tell whether the session changed meanwhile"
                  : "",
            );
          }
        } else if (message.type === "resume") {
          pane.v2 = true;
          pane.epoch = message.epoch;
          pane.drawn = true;
          initCursor();
          settle("");
          pane.claim();
        } else if (message.type === "frame") {
          pane.seq = message.seq;
          follow(message.cols, message.rows);
          owed = message.bytes;
          owedDone = () => scheduleClip(true);
        } else if (message.type === "pong") {
          // Liveness only (heardAt).
        } else if (message.type === "ack") {
          acked(message.id);
        } else if (message.type === "nack") {
          refused(message.id, !!message.retry);
        } else if (message.type === "size") {
          // Another viewer (or this pane) resized the PTY: what follows is
          // drawn at that size. Panes that do not claim it follow; a
          // claiming one too (last resizer wins) until it fits again.
          follow(message.cols, message.rows);
        } else if (message.type === "reset") {
          term.reset();
        } else if (message.type === "exit") {
          onExit(id, message);
        } else if (message.type === "error") {
          showBanner(message.message, [], true);
        }
      };
      // Strictly in order: an inflated message resolves later than a
      // text one after it would otherwise run.
      let chain = Promise.resolve();
      socket.onmessage = (event) => {
        heardAt = performance.now();
        pingAt = 0;
        const data = event.data;
        chain = chain
          .then(() => (pane.socket !== socket ? null : typeof data === "string" ? onText(data) : onBinary(data)))
          .catch((error) => {
            // A message that does not decode: this link's state is lost;
            // a fresh socket starts over with a snapshot.
            if (pane.socket !== socket) return;
            console.error(error);
            pane.seq = null;
            socket.close(4000);
          });
      };
      const closed = (event) => {
        clearInterval(watchdog);
        if (pane.socket !== socket) return;
        pane.socket = null;
        pane.ready = false;
        if (pane.inflater) pane.inflater.close();
        pane.inflater = null;
        requeueInflight();
        renderPending();
        if (!pane.lostAt) pane.lostAt = Date.now();
        endReplay();
        // A snapshot or frame cut short: the screen is not at `seq`.
        if (owed > 0) pane.seq = null;
        // 4001: the hub's token was rotated; 4002: this browser's login
        // ended (signed out elsewhere, expired). Either way: sign in again.
        if (event.code === 4001 || event.code === 4002) {
          const why = event.code === 4001 ? "rotated" : "ended";
          window.location.replace("/?signed_out=" + why + window.location.hash);
          return;
        }
        if (pane.sessionId !== id || pane.disposed) return;
        if (event.code === 4005) {
          // Relaunched in place (here, elsewhere, or by the CLI): a new
          // process and stream; attach it.
          ctx.refresh().catch(() => {}).finally(() => {
            if (pane.sessionId === id && !pane.socket && !pane.disposed) pane.show(id);
          });
          return;
        }
        if (pane.exited) return;
        if (event.code === 1012) {
          // A live handoff (`fw hub upgrade`): the session carries on in
          // the next hub process; reconnect at once, quietly.
          pane.refocus = host.contains(document.activeElement);
          setLink("connecting", "hub restarting");
          showBanner("The hub is restarting (upgrade); the session keeps running. Reconnecting…", [], true);
          pane.retryTimer = setTimeout(() => pane.reconnectNow(), 300);
          return;
        }
        if (event.code === 4011 && ctx.machineLink && ctx.machineLink(id)) {
          // Its machine was re-paired (still registered): reconnect
          // through the new registration.
          scheduleReconnect(id, event.code);
          return;
        }
        if (event.code === 4010 || event.code === 4011) {
          // A machine's session: that machine refused this hub's
          // credential (revoked there) or was removed from this hub. The
          // last screen stays; retrying waits for a fix.
          setLink("lost", event.code === 4010 ? "machine refused" : "machine removed");
          showBanner(
            event.code === 4010
              ? `${event.reason || "The machine refused this hub's credential"}. Re-pair it (Machines in the palette), then reconnect.`
              : "This session's machine was removed from this hub.",
            event.code === 4010
              ? [el("button", { type: "button", className: "slab", onclick: () => pane.reconnectNow() }, "Reconnect")]
              : [],
          );
          return;
        }
        scheduleReconnect(id, event.code);
      };
      socket.onclose = closed;
    }

    function onExit(id, message) {
      const how = ctx.exitHow(message);
      pane.exited = message;
      pane.term.write(`\r\n\x1b[2m[process ended: ${how}]\x1b[0m\r\n`);
      setLink("exited", "ended");
      showBanner(
        `Process ended (${how}). Scrollback stays until the session is closed or relaunched.`,
        endedActions(id),
        !message.signal && message.code === 0,
      );
      ctx.onExit(pane);
    }

    function scheduleReconnect(id, code) {
      pane.retry += 1;
      // A blip first: retried every RETRY_FAST_MS or sooner for a while,
      // then backing off to 10 s.
      const fast = Date.now() - (pane.lostAt || Date.now()) < RETRY_FAST_FOR;
      const cap = fast ? RETRY_FAST_MS : 10000;
      const delay = Math.min(cap, 500 * 2 ** Math.min(pane.retry - 1, 5));
      const due = Date.now() + delay;
      setLink("lost", pane.queue.length ? `link lost · ${pane.queue.length} queued` : "link lost");
      const now = el("button", { type: "button", className: "slab", onclick: () => pane.reconnectNow() }, "Reconnect now");
      const tick = () => {
        const seconds = Math.max(0, Math.ceil((due - Date.now()) / 1000));
        const machine = ctx.machineLink ? ctx.machineLink(id) : null;
        const why = state.offline
          ? "The hub is unreachable."
          : machine && (code === 1011 || (machine.link !== "online" && machine.link !== "connecting"))
            ? `Machine ${machine.name} is ${machine.link === "refused" ? "refusing this hub" : "unreachable"}; this is its last screen.`
            : code === "silent"
              ? "The link stopped answering; the session keeps running on the hub."
              : `Link lost (code ${code}); the session keeps running on the hub.`;
        const queued = pane.queue.length;
        const waiting = queued
          ? ` ${queued} keystroke${queued === 1 ? "" : "s"} queued: sent once the link is back.`
          : "";
        const refused = pane.refused ? ` ${pane.refused} more not kept (the queue is full).` : "";
        bannerText.textContent = `${why} Reconnecting in ${seconds}s.${waiting}${refused}`;
        setLink("lost", queued ? `link lost · ${queued} queued` : "link lost");
      };
      pane.tick = tick;
      showBanner("", [now]);
      tick();
      pane.countdown = setInterval(tick, 250);
      pane.retryTimer = setTimeout(() => pane.reconnectNow(), delay);
    }

    pane.reconnectNow = async () => {
      stopRetry();
      const id = pane.sessionId;
      if (!id || pane.socket || pane.disposed) return;
      const known = sessionById(id);
      if (state.offline === 0 && known && known.live) {
        // Live when last listed: the socket at once (a round trip saved
        // on a slow link); the re-list settles a session gone meanwhile.
        ctx.refresh().catch(() => {});
        connect(id);
        return;
      }
      try {
        await ctx.refresh();
      } catch (_) {
        // Hub unreachable: try the socket anyway; its failure reschedules.
      }
      // The refresh may have settled it already (session gone or closed).
      if (pane.sessionId !== id || pane.exited || pane.socket || pane.disposed) return;
      const session = sessionById(id);
      if (state.offline === 0 && !session) {
        ctx.onSessionGone(pane, id);
        return;
      }
      if (state.offline === 0 && !session.live) {
        pane.show(id, true);
        return;
      }
      connect(id);
    };

    // A dropped link comes back now (network back, tab visible again).
    pane.wake = () => {
      if (pane.sessionId && !pane.socket && !pane.exited) pane.reconnectNow();
    };

    pane.focus = () => {
      if (pane.term) pane.term.focus();
    };

    pane.dispose = () => {
      pane.disposed = true;
      if (pane.resizeObserver) pane.resizeObserver.disconnect();
      pane.resizeObserver = null;
      clearTimeout(retryTimer);
      closeSocket();
      forgetLink();
      if (pane.term) pane.term.dispose();
      pane.term = null;
      pane.fit = null;
      root.remove();
    };

    return pane;
  }

  return { create };
};

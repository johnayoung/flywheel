// flywheel hub: one pane = one xterm attached to one session over its own
// WebSocket (protocol: crates/flywheel-hub/src/server.rs), with its own
// fit, reconnect-with-backoff, banner, and link state. app.js lays panes
// out (tabs, splits) and decides which one has focus.
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
    const bannerActions = el("span", { className: "banner-actions" });
    const banner = el("div", { className: "banner pane-banner", hidden: true, role: "status" }, bannerText, bannerActions);
    const root = el("section", { className: "pane", dataset: { pane: paneId } }, head, el("div", { className: "pane-body" }, host), banner);

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
      size: null, // {cols, rows} the PTY has, as the server last said
      following: false, // sized to the PTY, not to its box (another viewer's size)
      retry: 0,
      retryTimer: null,
      countdown: null,
      tick: null,
      dropped: 0, // keystrokes typed while the link was down
      link: { kind: "idle", text: "idle" },
      disposed: false,
    };

    function makeTerm() {
      const term = new window.Terminal({
        cursorBlink: true,
        fontFamily: ctx.TERM_FONT,
        fontSize: ctx.fontSize(),
        lineHeight: 1.12,
        scrollback: 10000,
        allowProposedApi: false,
        ...ctx.termTheme(),
      });
      const fit = new window.FitAddon.FitAddon();
      term.loadAddon(fit);
      term.loadAddon(new window.WebLinksAddon.WebLinksAddon());
      term.open(host);
      muteQueries(term);
      term.onData((data) => pane.sendInput(JSON.stringify({ type: "input", data: ctx.transformInput(data) })));
      term.onBinary((data) => {
        const bytes = new Uint8Array(data.length);
        for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i) & 0xff;
        pane.sendInput(bytes);
      });
      term.onResize(({ cols, rows }) => {
        // Only a claiming pane's own fit resizes the PTY; following the
        // server's size must not echo it back.
        if (!pane.following && pane.claims()) pane.send(JSON.stringify({ type: "resize", cols, rows }));
        ctx.onResize(pane);
        scheduleClip();
      });
      term.onCursorMove(scheduleClip);
      term.onScroll(scheduleClip);
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
      new ResizeObserver(refit).observe(host);
    }

    pane.claims = () => ctx.claims(pane);

    // A terminal taller than its box (following another viewer's size, or
    // a phone's row floor) is clipped. The clip window follows the cursor:
    // a prompt at the top of a fresh shell stays in view as well as one at
    // the bottom of a busy agent. Rows cut off are flagged above/below.
    let clipFrame = 0;
    function scheduleClip() {
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
        const pad = parseFloat(getComputedStyle(host).paddingTop) || 0;
        const avail = host.clientHeight - pad;
        const height = xt.offsetHeight;
        overflow = Math.max(0, height - avail);
        if (overflow > 0 && pane.term.rows > 0) {
          const rowH = height / pane.term.rows;
          const buf = pane.term.buffer.active;
          const row = buf.cursorY + buf.baseY - buf.viewportY;
          shift = Math.min(overflow, Math.max(0, (row + 1) * rowH - avail));
        }
      }
      xt.style.transform = shift > 0 ? `translateY(${-shift}px)` : "";
      host.classList.toggle("clip-above", shift > 0.5);
      host.classList.toggle("clip-below", overflow - shift > 0.5);
    }

    // Size the terminal: to the box while claiming (the PTY follows),
    // else to the PTY's size (clipped, bottom rows in view).
    pane.refit = () => {
      if (!pane.term || pane.replaying || pane.disposed) return;
      // A divider is being dragged: fit once on release (app.js), not on
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

    // Keystrokes cannot be queued safely (a reconnect replays a screen they
    // were not typed against), so they are dropped -- and the banner says so.
    pane.sendInput = (message) => {
      if (pane.send(message) || !pane.sessionId || pane.exited) return;
      pane.dropped += 1;
      if (pane.tick) pane.tick();
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
        pane.socket.close(1000);
        pane.socket = null;
      }
      endReplay();
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
      pane.dropped = 0;
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
      pane.sessionId = null;
      pane.exited = null;
      pane.size = null;
      if (pane.term) pane.term.reset();
      pane.hideBanner();
      setLink("idle");
    };

    // Open (or re-open) the socket. Each connect replays a snapshot of the
    // session's terminal, so the terminal is reset first.
    function connect(id) {
      stopRetry();
      setLink("connecting", pane.retry ? `retry ${pane.retry}` : "connecting");
      const scheme = window.location.protocol === "https:" ? "wss" : "ws";
      const socket = new WebSocket(`${scheme}://${window.location.host}/api/sessions/${id}/ws`);
      socket.binaryType = "arraybuffer";
      pane.socket = socket;
      pane.replaying = false;
      const term = pane.term;
      socket.onopen = () => {
        // The snapshot follows; clear only now, so a dead link keeps the
        // last screen visible while it retries.
        term.reset();
        pane.retry = 0;
        pane.dropped = 0;
        setLink("attached");
        pane.sendTheme();
        pane.hideBanner();
        ctx.onAttached(pane);
      };
      // The snapshot is drawn at the session's size: replay exactly its
      // bytes at that size, then size the terminal by the rules (a
      // claiming pane resizes the session; the program redraws).
      let pending = 0;
      const replayed = () => {
        if (pane.socket !== socket) return;
        pane.replaying = false;
        initCursor();
        pane.claim();
      };
      socket.onmessage = (event) => {
        if (typeof event.data !== "string") {
          const bytes = new Uint8Array(event.data);
          if (pending > 0) {
            pending -= bytes.length;
            term.write(bytes, pending <= 0 ? replayed : undefined);
          } else {
            term.write(bytes);
          }
          return;
        }
        let message;
        try {
          message = JSON.parse(event.data);
        } catch (_) {
          return;
        }
        if (message.type === "snapshot") {
          pane.replaying = true;
          pane.size = { cols: message.cols, rows: message.rows };
          pane.following = true;
          term.resize(message.cols, message.rows);
          pane.following = false;
          pending = message.bytes;
          if (!(pending > 0)) replayed();
        } else if (message.type === "size") {
          // Another viewer (or this pane) resized the PTY: what follows is
          // drawn at that size. Panes that do not claim it follow; a
          // claiming one too (last resizer wins) until it fits again.
          pane.size = { cols: message.cols, rows: message.rows };
          if (!pane.replaying && (term.cols !== message.cols || term.rows !== message.rows)) {
            pane.following = true;
            term.resize(message.cols, message.rows);
            pane.following = !pane.claims();
            host.classList.toggle("following", pane.following);
          }
          ctx.onResize(pane);
          scheduleClip();
        } else if (message.type === "reset") {
          term.reset();
        } else if (message.type === "exit") {
          onExit(id, message);
        } else if (message.type === "error") {
          showBanner(message.message, [], true);
        }
      };
      socket.onclose = (event) => {
        if (pane.socket !== socket) return;
        pane.socket = null;
        endReplay();
        // 4001: the hub's token was rotated; 4002: this browser's login
        // ended (signed out elsewhere, expired). Either way: sign in again.
        if (event.code === 4001 || event.code === 4002) {
          const why = event.code === 4001 ? "rotated" : "ended";
          window.location.replace("/?signed_out=" + why + window.location.hash);
          return;
        }
        if (pane.exited || pane.sessionId !== id || pane.disposed) return;
        scheduleReconnect(id, event.code);
      };
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
      const delay = Math.min(10000, 500 * 2 ** Math.min(pane.retry - 1, 5));
      const due = Date.now() + delay;
      setLink("lost", "link lost");
      const now = el("button", { type: "button", className: "slab", onclick: () => pane.reconnectNow() }, "Reconnect now");
      const tick = () => {
        const seconds = Math.max(0, Math.ceil((due - Date.now()) / 1000));
        const why = state.offline
          ? "The hub is unreachable."
          : `Link lost (code ${code}); the session keeps running on the hub.`;
        const dropped = pane.dropped
          ? ` ${pane.dropped} keystroke${pane.dropped === 1 ? "" : "s"} not sent.`
          : "";
        bannerText.textContent = `${why} Reconnecting in ${seconds}s.${dropped}`;
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
      closeSocket();
      if (pane.term) pane.term.dispose();
      pane.term = null;
      pane.fit = null;
      root.remove();
    };

    return pane;
  }

  return { create };
};

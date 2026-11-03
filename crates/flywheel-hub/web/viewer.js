// flywheel hub: a share link's read-only viewer (`/share/<credential>`).
// One session's terminal stream (GET .../ws) and its title and status
// (GET .../info, every few seconds) -- the only things the link reaches.
// It never sends input or a size: the hub would refuse them anyway.
// Reconnects after a hub upgrade or a dropped link; stops for good once
// the link is revoked or expires.
"use strict";

(function () {
  const credential = window.location.pathname.split("/")[2] || "";
  const base = `/share/${credential}`;
  const $ = (id) => document.getElementById(id);
  const stage = $("vw-stage");
  const view = window.FwHubTermView($("vw-term"), stage);
  const INFO_MS = 4000;

  const state = {
    socket: null,
    backoff: 500,
    retry: 0,
    over: false, // revoked, expired, or removed: no more reconnects
    expires: 0, // unix seconds
    skew: 0, // this clock minus the hub's, seconds
    exited: false,
  };

  function note(text, bad) {
    const box = $("vw-note");
    box.hidden = !text;
    box.textContent = text || "";
    box.classList.toggle("bad", !!bad);
  }

  function link(stateName, label) {
    const pill = $("vw-link");
    pill.dataset.link = stateName;
    pill.textContent = label;
  }

  function span(seconds) {
    if (seconds < 60) return `${Math.max(0, Math.floor(seconds))}s`;
    if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
    if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
    return `${Math.floor(seconds / 86400)}d ${Math.floor((seconds % 86400) / 3600)}h`;
  }

  // The link is over: nothing on screen may still read as live.
  function end(message) {
    state.over = true;
    clearTimeout(state.retry);
    link("ended", "ended");
    note(message, true);
    const pill = $("vw-status");
    pill.hidden = true;
    pill.textContent = "";
    $("vw-what").textContent = "No longer live: the screen is as it last was.";
    $("vw-meta").textContent = message.replace(/^This share link (was|has) /, "link ").replace(/\.$/, "");
    if (state.socket) state.socket.close();
  }

  function exitText(code, signal) {
    if (signal) return `killed by ${signal}`;
    if (code === null || code === undefined) return "exit code unknown";
    return `exit code ${code}`;
  }

  async function info() {
    if (state.over) return;
    let response;
    try {
      response = await fetch(`${base}/info`, { credentials: "omit", cache: "no-store" });
    } catch (_) {
      return; // the socket's reconnect says the hub is unreachable
    }
    if (response.status === 404 || response.status === 410) {
      end(response.status === 410 ? "This share link has expired." : "This share link was revoked.");
      return;
    }
    if (!response.ok) return;
    const data = await response.json();
    state.expires = data.share.expires;
    state.skew = Date.now() / 1000 - data.share.now;
    const session = data.session;
    if (!session) {
      end("The shared session was removed.");
      return;
    }
    const title = session.title ? `${session.name}: ${session.title}` : session.name;
    $("vw-title").textContent = session.name;
    $("vw-title").title = title;
    document.title = `${title} - shared terminal`;
    const pill = $("vw-status");
    const agent = session.agent && session.agent.status;
    let label = session.state;
    if (session.state === "running" && agent) label = agent;
    else if (session.state === "exited") label = `exited (${exitText(session.code, session.signal)})`;
    pill.hidden = !label;
    pill.textContent = label || "";
    pill.dataset.status = agent && session.state === "running" ? agent : session.state;
    pill.title = session.agent && session.agent.detail ? session.agent.detail : "";
    renderMeta(session);
  }

  function renderMeta(session) {
    const left = state.expires - (Date.now() / 1000 - state.skew);
    const parts = [];
    if (session) {
      parts.push([session.workspace, session.kind].filter(Boolean).join(" · "));
      if (session.title) parts.push(session.title);
    }
    parts.push(left > 0 ? `link expires in ${span(left)}` : "link expired");
    $("vw-meta").textContent = parts.filter(Boolean).join("  ·  ");
    state.lastSession = session || state.lastSession;
  }

  function connect() {
    if (state.over) return;
    const scheme = window.location.protocol === "https:" ? "wss" : "ws";
    const socket = new WebSocket(`${scheme}://${window.location.host}${base}/ws`);
    socket.binaryType = "arraybuffer";
    state.socket = socket;
    link("connecting", "connecting");
    socket.onopen = () => {
      state.backoff = 500;
      link("live", "live");
    };
    socket.onmessage = (event) => {
      if (typeof event.data !== "string") {
        view.write(new Uint8Array(event.data));
        return;
      }
      let message;
      try {
        message = JSON.parse(event.data);
      } catch (_) {
        return;
      }
      switch (message.type) {
        case "snapshot":
          // A fresh replay: the screen as it is, at its size.
          view.reset();
          view.resize(message.cols, message.rows);
          if (!state.exited) note("");
          break;
        case "size":
          view.resize(message.cols, message.rows);
          break;
        case "reset":
          view.reset();
          break;
        case "exit":
          state.exited = true;
          note(`The process ended (${exitText(message.code, message.signal)}).`);
          info();
          break;
        default:
          // `error`: this view sends nothing, so nothing to refuse.
          break;
      }
    };
    socket.onclose = (event) => {
      if (state.socket !== socket) return;
      state.socket = null;
      if (state.over) return;
      if (event.code === 4004) {
        end(event.reason === "share link expired" ? "This share link has expired." : "This share link was revoked.");
        return;
      }
      // 1012: the hub is being upgraded -- back at once. Else back off.
      const wait = event.code === 1012 ? 150 : state.backoff;
      state.backoff = Math.min(state.backoff * 2, 15000);
      link("connecting", event.code === 1012 ? "hub upgrading" : "reconnecting");
      clearTimeout(state.retry);
      state.retry = setTimeout(() => {
        info().finally(connect);
      }, wait);
    };
  }

  // Fit the whole screen to the window, or keep it readable and scroll.
  const fitButton = $("vw-fit");
  fitButton.addEventListener("click", () => view.toggleMode());
  view.onMode((mode) => {
    fitButton.textContent = mode === "fit" ? "fit" : "readable";
    fitButton.setAttribute("aria-pressed", mode === "fit" ? "true" : "false");
  });

  info().finally(connect);
  setInterval(info, INFO_MS);
  setInterval(() => {
    if (!state.over && state.expires) renderMeta(state.lastSession);
  }, 1000);
  window.addEventListener("online", () => {
    if (!state.socket && !state.over) {
      clearTimeout(state.retry);
      connect();
    }
  });
})();

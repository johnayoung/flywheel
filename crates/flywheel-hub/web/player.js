// flywheel hub: the replay player (`/replay#<recording>`). Loads an
// asciinema v2 recording (GET /api/recordings/<name>) and plays it in a
// watch-only terminal: play / pause, speed, seek, and "skip idle" (pauses
// longer than IDLE_CAP play as IDLE_CAP). Seeking replays every event up
// to the target at once from a cleared terminal. Keys: space play/pause,
// left / right 5 s back / forward, Home to the start.
"use strict";

(function () {
  const $ = (id) => document.getElementById(id);
  const name = decodeURIComponent(window.location.hash.slice(1));
  const view = window.FwHubTermView($("vw-term"), $("vw-stage"));
  const IDLE_CAP = 2;
  const CONTINUED = /^continued in (\S+\.cast)$/;
  const SEEK_STEP = 5;

  const play = {
    header: null,
    events: [], // { t (as recorded), at (as played), code, data }
    duration: 0,
    pos: 0, // seconds on the played timeline
    next: 0, // index of the next event to apply
    playing: false,
    speed: 1,
    frame: 0,
    last: 0, // performance.now() of the last frame
  };

  function note(text, bad) {
    const box = $("vw-note");
    box.hidden = !text;
    box.textContent = text || "";
    box.classList.toggle("bad", !!bad);
  }

  function clock(seconds) {
    const s = Math.max(0, Math.floor(seconds));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const pad = (n) => String(n).padStart(2, "0");
    return h ? `${h}:${pad(m)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
  }

  function parse(text) {
    const lines = text.split("\n").filter((line) => line.trim());
    if (!lines.length) throw new Error("the recording is empty");
    const header = JSON.parse(lines[0]);
    if (header.version !== 2) throw new Error(`not an asciinema v2 recording (version ${header.version})`);
    const events = [];
    for (let i = 1; i < lines.length; i += 1) {
      let event;
      try {
        event = JSON.parse(lines[i]);
      } catch (_) {
        // A recording still being written may end mid-line.
        continue;
      }
      if (Array.isArray(event) && event.length >= 3 && typeof event[0] === "number") {
        events.push({ t: event[0], at: event[0], code: String(event[1]), data: String(event[2]) });
      }
    }
    return { header, events };
  }

  // The played timeline: recorded times, with idle gaps capped.
  function retime() {
    const skip = $("pl-idle").checked;
    let at = 0;
    let prev = 0;
    for (const event of play.events) {
      const gap = Math.max(0, event.t - prev);
      at += skip ? Math.min(gap, IDLE_CAP) : gap;
      prev = event.t;
      event.at = at;
    }
    play.duration = play.events.length ? play.events[play.events.length - 1].at : 0;
  }

  // Apply events up to `until` (played seconds), output batched.
  function advance(until) {
    let batch = "";
    while (play.next < play.events.length && play.events[play.next].at <= until) {
      const event = play.events[play.next];
      if (event.code === "o") {
        batch += event.data;
      } else if (event.code === "r") {
        const [c, r] = event.data.split("x").map((n) => parseInt(n, 10));
        if (batch) view.write(batch);
        batch = "";
        if (c > 0 && r > 0) view.resize(c, r);
      } else if (event.code === "m" && /^exit/.test(event.data)) {
        if (batch) view.write(batch);
        batch = "";
        note(`The process ended (${event.data.replace(/^exit:?\s*/, "") || "exit"}).`);
      } else if (event.code === "m" && CONTINUED.test(event.data)) {
        if (batch) view.write(batch);
        batch = "";
        note("This part ends here: the recording continues in the next part.");
      }
      play.next += 1;
    }
    if (batch) view.write(batch);
  }

  function seek(target) {
    const to = Math.max(0, Math.min(target, play.duration));
    note("");
    view.reset();
    view.resize(play.header.width, play.header.height);
    play.next = 0;
    play.pos = to;
    advance(to);
    render();
  }

  function render() {
    $("pl-time").textContent = `${clock(play.pos)} / ${clock(play.duration)}`;
    if (!play.dragging) {
      $("pl-seek").value = play.duration ? Math.round((play.pos / play.duration) * 1000) : 0;
    }
    $("pl-play").textContent = play.playing ? "Pause" : play.pos >= play.duration && play.duration ? "Replay" : "Play";
  }

  function tick(now) {
    if (!play.playing) return;
    const dt = (now - play.last) / 1000;
    play.last = now;
    play.pos = Math.min(play.duration, play.pos + dt * play.speed);
    advance(play.pos);
    if (play.pos >= play.duration) {
      play.playing = false;
      // Played to its end: carry on in the next part.
      if (play.nextPart) {
        window.location.replace(`/replay#${encodeURIComponent(play.nextPart)}`);
        return;
      }
    }
    render();
    if (play.playing) play.frame = window.requestAnimationFrame(tick);
  }

  // `<base>.cast` is part 1, `<base>.partN.cast` part N.
  function partName(part) {
    const base = name.replace(/(\.part\d+)?\.cast$/, "");
    return part <= 1 ? `${base}.cast` : `${base}.part${part}.cast`;
  }

  // The part links, and a tick on the bar for every marker.
  function renderParts() {
    const hub = play.header.x_fw_hub || {};
    const part = Number(hub.part) || 1;
    const continued = play.events.find((e) => e.code === "m" && CONTINUED.test(e.data));
    play.nextPart = continued ? CONTINUED.exec(continued.data)[1] : "";
    const prev = $("pl-prev");
    prev.hidden = part <= 1;
    if (part > 1) prev.href = `/replay#${encodeURIComponent(partName(part - 1))}`;
    const next = $("pl-next");
    next.hidden = !play.nextPart;
    if (play.nextPart) next.href = `/replay#${encodeURIComponent(play.nextPart)}`;
    const label = $("pl-part");
    label.hidden = part <= 1 && !play.nextPart;
    label.textContent = `part ${part}`;
    const marks = $("pl-marks");
    marks.textContent = "";
    if (!play.duration) return;
    for (const event of play.events) {
      if (event.code !== "m") continue;
      const kind = /^exit/.test(event.data) ? "exit" : CONTINUED.test(event.data) ? "continued" : "marker";
      const tick = document.createElement("button");
      tick.type = "button";
      tick.className = "pl-mark";
      tick.dataset.kind = kind;
      tick.style.left = `${(event.at / play.duration) * 100}%`;
      tick.title = `${clock(event.at)}  ${event.data}`;
      tick.setAttribute("aria-label", tick.title);
      tick.addEventListener("click", () => {
        const wasPlaying = play.playing;
        pause();
        seek(event.at);
        if (wasPlaying && play.pos < play.duration) start();
      });
      marks.appendChild(tick);
    }
  }

  function start() {
    if (play.pos >= play.duration) seek(0);
    play.playing = true;
    play.last = performance.now();
    play.frame = window.requestAnimationFrame(tick);
    render();
  }

  function pause() {
    play.playing = false;
    window.cancelAnimationFrame(play.frame);
    render();
  }

  function toggle() {
    if (play.playing) pause();
    else start();
  }

  async function load() {
    if (!name) {
      note("No recording named: open one from the hub's Sharing & recordings.", true);
      return;
    }
    const path = `/api/recordings/${encodeURIComponent(name)}`;
    $("pl-download").href = path;
    $("pl-download").setAttribute("download", name);
    let text;
    try {
      const response = await fetch(path, { credentials: "same-origin", cache: "no-store" });
      if (response.status === 401) {
        window.location.replace("/");
        return;
      }
      if (!response.ok) {
        let message = `${response.status} ${response.statusText}`;
        try {
          message = (await response.json()).error || message;
        } catch (_) {
          // not JSON
        }
        note(`Cannot load ${name}: ${message}`, true);
        return;
      }
      text = await response.text();
    } catch (error) {
      note(`Cannot load ${name}: ${error.message}`, true);
      return;
    }
    let parsed;
    try {
      parsed = parse(text);
    } catch (error) {
      note(`${name}: ${error.message}`, true);
      return;
    }
    play.header = parsed.header;
    play.events = parsed.events;
    retime();
    renderParts();
    const title = parsed.header.title || name;
    $("vw-title").textContent = title;
    document.title = `${title} - replay`;
    view.resizeNow(parsed.header.width || 80, parsed.header.height || 24);
    $("pl-play").disabled = false;
    $("pl-seek").disabled = false;
    render();
    start();
  }

  $("pl-play").addEventListener("click", toggle);
  $("pl-speed").addEventListener("change", (event) => {
    play.speed = parseFloat(event.target.value) || 1;
  });
  $("pl-idle").addEventListener("change", () => {
    // Keep the place: the same event stays next.
    const at = play.next > 0 ? play.events[play.next - 1].t : 0;
    retime();
    renderParts();
    const event = play.events.find((e) => e.t >= at);
    play.pos = event ? Math.min(event.at, play.duration) : play.duration;
    render();
  });
  const bar = $("pl-seek");
  bar.addEventListener("input", () => {
    play.dragging = true;
    $("pl-time").textContent = `${clock((bar.value / 1000) * play.duration)} / ${clock(play.duration)}`;
  });
  bar.addEventListener("change", () => {
    play.dragging = false;
    // Read first: pausing redraws the bar at the old position.
    const target = (bar.value / 1000) * play.duration;
    const wasPlaying = play.playing;
    pause();
    seek(target);
    if (wasPlaying && play.pos < play.duration) start();
  });
  document.addEventListener("keydown", (event) => {
    if (!play.header || event.target.closest("select, input:not([type=range])")) return;
    if (event.key === " ") {
      event.preventDefault();
      toggle();
    } else if (event.key === "ArrowRight" || event.key === "ArrowLeft") {
      event.preventDefault();
      const wasPlaying = play.playing;
      pause();
      seek(play.pos + (event.key === "ArrowRight" ? SEEK_STEP : -SEEK_STEP));
      if (wasPlaying && play.pos < play.duration) start();
    } else if (event.key === "Home") {
      event.preventDefault();
      pause();
      seek(0);
    }
  });
  // Fit the whole screen to the window, or keep it readable and scroll.
  const fitButton = $("vw-fit");
  fitButton.addEventListener("click", () => view.toggleMode());
  view.onMode((mode) => {
    fitButton.textContent = mode === "fit" ? "fit" : "readable";
    fitButton.setAttribute("aria-pressed", mode === "fit" ? "true" : "false");
  });
  window.addEventListener("hashchange", () => window.location.reload());
  load();
})();

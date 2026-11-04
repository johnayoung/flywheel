// flywheel hub: browser notifications. When an agent becomes blocked or
// finishes a turn while this page is hidden or unfocused, this device
// shows a system notification; clicking it opens that session. Off until
// turned on per device (the "alerts" chip or the palette), which asks the
// browser's permission from that tap.
//
// Server push (ntfy-style webhook, `fw hub serve --notify-url`) is the
// hub's own and works with every browser closed; GET /api/notify says
// whether it is on, POST /api/notify/test sends a test.
"use strict";

window.FwHubNotify = function (ctx) {
  const { $, state, sessionById, workspaceById, pwa, api, toast, openSession, present, attention } = ctx;

  const supported = "Notification" in window;
  const IS_IOS = /iphone|ipad|ipod/i.test(navigator.userAgent) ||
    (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
  // One notification per session and status at most this often; a new
  // spell inside the window is held until it ends, then shown if it still
  // stands (back-to-back permission prompts each reach you).
  const DEBOUNCE_MS = 15000;
  const recent = new Map(); // "id:status" -> ms
  const held = new Map(); // id -> timer
  let push = undefined; // the hub's webhook push: null (off) or {host, events}

  const permission = () => (supported ? Notification.permission : "unsupported");
  // Per device (a browser permission): Settings' notifications.alerts.
  const isOn = () => supported && ctx.prefs.get("notifications.alerts", false) === true && permission() === "granted";

  function renderChip() {
    const chip = $("#alerts-toggle");
    if (!chip) return;
    const on = isOn();
    const denied = permission() === "denied";
    chip.classList.toggle("on", on);
    chip.classList.toggle("blocked", !on && denied);
    chip.setAttribute("aria-pressed", String(on));
    const label = on ? "alerts on" : denied ? "alerts blocked" : "alerts off";
    $("#alerts-label").textContent = label;
    const server = push ? `; push to ${push.host} (${push.events.join(", ")}) is on for the hub` : "";
    // The title leads with the state: a desktop sidebar shows only the dot.
    chip.title = on
      ? `Alerts on: this device is notified when an agent is blocked or done while the hub is in the background${server}`
      : denied
        ? `Alerts blocked: this browser refused notifications for the hub (allow them in its site settings)${server}`
        : `Alerts off: notify this device when an agent is blocked or done${server}`;
  }

  async function toggle() {
    if (isOn()) {
      ctx.prefs.set("notifications.alerts", false, { device: true });
      renderChip();
      toast("Alerts off on this device.");
      return;
    }
    if (!supported) {
      toast(IS_IOS && !pwa.standalone()
        ? "On iPhone and iPad, notifications need the hub on the Home Screen: Share, then Add to Home Screen, and turn alerts on there."
        : "This browser cannot show notifications.", { tone: "bad", timeout: 9000 });
      return;
    }
    if (!window.isSecureContext) {
      toast("Notifications need https (or localhost): reach the hub through tailscale serve.", { tone: "bad", timeout: 9000 });
      return;
    }
    let answer = permission();
    if (answer === "default") {
      try {
        answer = await Notification.requestPermission();
      } catch (_) {
        answer = permission();
      }
    }
    if (answer !== "granted") {
      renderChip();
      toast("Notifications are blocked for this site: allow them in the browser's site settings, then try again.", { tone: "bad", timeout: 9000 });
      return;
    }
    ctx.prefs.set("notifications.alerts", true, { device: true });
    await pwa.register();
    renderChip();
    toast("Alerts on: this device hears about blocked and finished agents while the hub is in the background.");
  }

  // Someone is at the page right now (visible, focused, not idle).
  const looking = () => (present ? present() : document.visibilityState === "visible" && document.hasFocus());

  // A pushed status change (`before`/`after`: {status, detail, since}).
  function onStatus(sessionId, before, after) {
    clearTimeout(held.get(sessionId));
    held.delete(sessionId);
    if (!after || !isOn()) return;
    const status = after.status;
    if (status !== "blocked" && status !== "done") return;
    // The same spell again (a detail or rollup update): already handled.
    if (before && before.status === status && before.since === after.since) return;
    consider(sessionId, after);
  }

  function consider(sessionId, agent) {
    if (looking()) return;
    const key = `${sessionId}:${agent.status}`;
    const wait = (recent.get(key) || 0) + DEBOUNCE_MS - Date.now();
    if (wait > 0) {
      held.set(sessionId, setTimeout(() => {
        held.delete(sessionId);
        const now = state.agent[sessionId];
        if (now && now.status === agent.status && now.since === agent.since && isOn()) consider(sessionId, now);
      }, wait));
      return;
    }
    recent.set(key, Date.now());
    show(sessionId, agent).catch(() => {});
  }

  // Another kind of news about a session (a pull request's CI): shown
  // like a status, under the same switch, debounce, and "not while
  // looking" rule.
  function alert(sessionId, title, body, urgent) {
    if (!isOn() || looking() || !sessionById(sessionId)) return;
    const key = `${sessionId}:${title}`;
    if ((recent.get(key) || 0) + DEBOUNCE_MS > Date.now()) return;
    recent.set(key, Date.now());
    display(sessionId, title, body, urgent).catch(() => {});
  }

  async function show(sessionId, agent) {
    const session = sessionById(sessionId);
    if (!session) return;
    const workspace = workspaceById(session.workspace_id);
    const branch = attention.branchOf(session);
    const where = `${session.machine ? `${session.machine}: ` : ""}${workspace ? `${workspace.name} / ${session.name}` : session.name}${branch ? ` (on ${branch})` : ""}`;
    const title = agent.status === "blocked" ? `${session.name} is blocked` : `${session.name} is done`;
    const body = agent.status === "blocked"
      ? `${where} waits on you${agent.detail ? `: ${agent.detail}` : "."}`
      : `${where} finished its turn.`;
    await display(sessionId, title, body, agent.status === "blocked");
  }

  async function display(sessionId, title, body, urgent) {
    const options = {
      body,
      tag: `fw-hub-${sessionId}`,
      renotify: true,
      icon: "/assets/icon-192.png",
      data: { session: sessionId },
      requireInteraction: !!urgent,
    };
    // Mobile Chrome allows notifications only through the service worker
    // (its click is relayed back as a message); desktop browsers without
    // one get a page notification.
    const registration = await pwa.register();
    if (registration && registration.showNotification) {
      await registration.showNotification(title, options);
      return;
    }
    const note = new Notification(title, options);
    note.onclick = () => {
      window.focus();
      note.close();
      open(sessionId);
    };
  }

  function open(sessionId) {
    if (typeof sessionId === "string" && sessionById(sessionId)) openSession(sessionId);
  }

  // Autopilot work in `folder` newly waits on a manual gate: a
  // notification that opens its Flywheel panel (where approve / deny are).
  async function gate(folder, label, count) {
    if (!isOn() || looking()) return;
    const key = `gate:${folder}`;
    if ((recent.get(key) || 0) + DEBOUNCE_MS > Date.now()) return;
    recent.set(key, Date.now());
    const title = `Autopilot in ${label} needs approval`;
    const options = {
      body: `${count} task${count === 1 ? "" : "s"} it emitted wait${count === 1 ? "s" : ""} on a manual gate: approve or deny in its Flywheel panel.`,
      tag: `fw-hub-gate-${label}`,
      renotify: true,
      icon: "/assets/icon-192.png",
      data: { folder },
      requireInteraction: true,
    };
    try {
      const registration = await pwa.register();
      if (registration && registration.showNotification) {
        await registration.showNotification(title, options);
        return;
      }
      const note = new Notification(title, options);
      note.onclick = () => {
        window.focus();
        note.close();
        ctx.openFolder(folder);
      };
    } catch (_) {
      // A browser that refuses this one: the Needs-you list still has it.
    }
  }

  pwa.onMessage((data) => {
    if (data.type !== "fw-hub-open") return;
    if (typeof data.folder === "string" && ctx.openFolder) ctx.openFolder(data.folder);
    else open(data.session);
  });

  async function loadPush() {
    try {
      push = (await api("GET", "/api/notify")).push;
    } catch (_) {
      push = null;
    }
    renderChip();
  }

  async function testPush() {
    try {
      await api("POST", "/api/notify/test");
      toast(`Test push sent to ${push ? push.host : "the webhook"}.`);
    } catch (error) {
      toast(`Test push failed: ${error.message}`, { tone: "bad", timeout: 9000 });
    }
  }

  function paletteItems(cmd) {
    cmd(
      isOn() ? "Alerts: stop notifying this device" : "Alerts: notify this device",
      "blocked or done, while in the background",
      toggle,
      "gear",
    );
    if (push) cmd(`Send a test push to ${push.host}`, "hub webhook", testPush, "send");
  }

  const chip = $("#alerts-toggle");
  if (chip) chip.addEventListener("click", toggle);
  renderChip();
  loadPush();

  return { onStatus, alert, gate, toggle, paletteItems, isOn, renderChip };
};

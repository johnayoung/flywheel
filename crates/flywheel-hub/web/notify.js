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
  const { $, state, store, sessionById, workspaceById, pwa, api, toast, openSession } = ctx;

  const supported = "Notification" in window;
  const IS_IOS = /iphone|ipad|ipod/i.test(navigator.userAgent) ||
    (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
  // One notification per session and status at most this often.
  const DEBOUNCE_MS = 15000;
  const recent = new Map(); // "id:status" -> ms
  let push = undefined; // the hub's webhook push: null (off) or {host, events}

  const permission = () => (supported ? Notification.permission : "unsupported");
  const isOn = () => supported && store.get("notify", false) === true && permission() === "granted";

  function renderChip() {
    const chip = $("#alerts-toggle");
    if (!chip) return;
    const on = isOn();
    const denied = permission() === "denied";
    chip.classList.toggle("on", on);
    chip.setAttribute("aria-pressed", String(on));
    $("#alerts-label").textContent = on ? "alerts on" : denied ? "alerts blocked" : "alerts off";
    const server = push ? `; push to ${push.host} (${push.events.join(", ")}) is on for the hub` : "";
    chip.title = on
      ? `This device is notified when an agent is blocked or done while the hub is in the background${server}`
      : `Notify this device when an agent is blocked or done${server}`;
  }

  async function toggle() {
    if (isOn()) {
      store.set("notify", false);
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
    store.set("notify", true);
    await pwa.register();
    renderChip();
    toast("Alerts on: this device hears about blocked and finished agents while the hub is in the background.");
  }

  // Someone is looking at the page right now.
  const looking = () => document.visibilityState === "visible" && document.hasFocus();

  // A pushed status change (`before`/`after`: {status, detail, since}).
  function onStatus(sessionId, before, after) {
    if (!after || !isOn() || looking()) return;
    const status = after.status;
    if (status !== "blocked" && status !== "done") return;
    if (before && before.status === status) return;
    const key = `${sessionId}:${status}`;
    const now = Date.now();
    if (recent.get(key) > now - DEBOUNCE_MS) return;
    recent.set(key, now);
    show(sessionId, after).catch(() => {});
  }

  async function show(sessionId, agent) {
    const session = sessionById(sessionId);
    if (!session) return;
    const workspace = workspaceById(session.workspace_id);
    const where = workspace ? `${workspace.name} / ${session.name}` : session.name;
    const title = agent.status === "blocked" ? `${session.name} is blocked` : `${session.name} is done`;
    const body = agent.status === "blocked"
      ? `${where} waits on you${agent.detail ? `: ${agent.detail}` : "."}`
      : `${where} finished its turn.`;
    const options = {
      body,
      tag: `fw-hub-${sessionId}`,
      renotify: true,
      icon: "/assets/icon-192.png",
      data: { session: sessionId },
      requireInteraction: agent.status === "blocked",
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

  pwa.onMessage((data) => {
    if (data.type === "fw-hub-open") open(data.session);
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

  return { onStatus, toggle, paletteItems, isOn };
};

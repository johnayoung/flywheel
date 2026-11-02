// flywheel hub: the installable-app glue. Registers the service worker
// (/sw.js: an offline app shell and notification clicks; it never touches
// the API or sign-in) where the browser allows one -- https or localhost --
// and relays its messages to the app.
"use strict";

window.FwHubPwa = function () {
  const supported = "serviceWorker" in navigator && window.isSecureContext;
  let ready = null;

  // The registration once active (null where unsupported or refused).
  function register() {
    if (!supported) return Promise.resolve(null);
    if (!ready) {
      ready = navigator.serviceWorker
        .register("/sw.js", { scope: "/" })
        .then(() => navigator.serviceWorker.ready)
        .catch(() => null);
    }
    return ready;
  }

  // Signing out drops the cached app shell.
  function forget() {
    if (supported && navigator.serviceWorker.controller) {
      navigator.serviceWorker.controller.postMessage({ type: "fw-hub-forget" });
    }
  }

  // Launched from the home screen (iOS reports it its own way).
  const standalone = () =>
    window.matchMedia("(display-mode: standalone)").matches || navigator.standalone === true;

  function onMessage(handler) {
    if (!supported) return;
    navigator.serviceWorker.addEventListener("message", (event) => handler(event.data || {}));
  }

  // Keep this signed-in page and its assets as the offline app shell.
  function keepShell(registration) {
    if (!registration || !registration.active) return;
    const urls = [
      ...[...document.querySelectorAll("script[src]")].map((s) => s.src),
      ...[...document.querySelectorAll('link[rel="stylesheet"], link[rel="preload"], link[rel="icon"]')].map((l) => l.href),
    ];
    registration.active.postMessage({ type: "fw-hub-shell", urls });
  }

  register().then(keepShell);
  document.documentElement.classList.toggle("standalone", standalone());

  return { supported, register, forget, standalone, onMessage };
};

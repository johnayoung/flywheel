// The CA onboarding page (/ca): is this device trusting the hub's CA yet?
// Probe the same host over https; trusted -> continue (with the one-time
// login code a QR carried in the URL fragment, which never reaches the
// network), else show the steps for this device's OS.
(function () {
  "use strict";
  const KEY = "fw-hub-ca-code";
  const params = new URLSearchParams(location.hash.slice(1));
  let code = params.get("code");
  if (code) {
    try { sessionStorage.setItem(KEY, code); } catch (_) { /* private mode */ }
    // Out of the address bar and history at once.
    history.replaceState(null, "", location.pathname);
  } else {
    try { code = sessionStorage.getItem(KEY); } catch (_) { code = null; }
  }
  const httpsBase = "https://" + location.host;
  const status = document.getElementById("status");
  const text = document.getElementById("status-text");
  const proceed = document.getElementById("continue");
  const recheck = document.getElementById("recheck");

  function osName() {
    const ua = navigator.userAgent || "";
    if (/iPhone|iPad|iPod/.test(ua) || (/Macintosh/.test(ua) && navigator.maxTouchPoints > 1)) return "ios";
    if (/Android/.test(ua)) return "android";
    if (/Windows/.test(ua)) return "windows";
    if (/Macintosh|Mac OS X/.test(ua)) return "mac";
    return "linux";
  }

  function openSteps() {
    const os = osName();
    for (const d of document.querySelectorAll("details[data-os]")) {
      d.open = d.dataset.os === os;
    }
    for (const a of document.querySelectorAll("a.dl[data-for]")) {
      a.classList.toggle("primary", a.dataset.for.split(" ").includes(os));
    }
  }

  async function trusted() {
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), 6000);
    try {
      // An opaque answer is enough: the fetch fails outright when the
      // certificate is not trusted.
      await fetch(httpsBase + "/ca/probe", { mode: "no-cors", cache: "no-store", signal: abort.signal });
      return true;
    } catch (_) {
      return false;
    } finally {
      clearTimeout(timer);
    }
  }

  function target() {
    return code ? httpsBase + "/login?code=" + encodeURIComponent(code) : httpsBase + "/";
  }

  function go() {
    try { sessionStorage.removeItem(KEY); } catch (_) { /* private mode */ }
    location.replace(target());
  }

  function show(state, message) {
    status.dataset.state = state;
    text.textContent = message;
  }

  async function check(auto) {
    show("checking", "Checking whether this device trusts the hub…");
    recheck.hidden = true;
    proceed.hidden = true;
    if (await trusted()) {
      show("trusted", code
        ? "This device trusts the hub. Signing you in over https…"
        : "This device trusts the hub.");
      proceed.textContent = code ? "Sign in" : "Open the hub";
      proceed.hidden = false;
      if (auto && location.protocol === "http:") go();
      return;
    }
    show("untrusted", "Not trusted yet: check the fingerprint, then install the CA below.");
    recheck.hidden = false;
    openSteps();
  }

  proceed.addEventListener("click", go);
  recheck.addEventListener("click", () => check(true));
  openSteps();
  check(true);
})();

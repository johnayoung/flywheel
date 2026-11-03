// The CA onboarding page (/ca): is this device trusting the hub's CA yet?
// Probe the same host over https; trusted -> offer the hub, else show the
// steps for this device's OS. No credential ever comes here: this page is
// plain http, which an active attacker on the LAN could rewrite, so the
// login code goes only to the https login (its own QR). The page never
// navigates away by itself, so a trusting device can still compare the
// fingerprint.
(function () {
  "use strict";
  // An older QR carried a code in the fragment: drop it unread.
  if (location.hash) history.replaceState(null, "", location.pathname);
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

  function go() {
    location.assign(httpsBase + "/");
  }

  function show(state, message) {
    status.dataset.state = state;
    text.textContent = message;
  }

  async function check() {
    show("checking", "Checking whether this device trusts the hub…");
    recheck.hidden = true;
    proceed.hidden = true;
    if (await trusted()) {
      show("trusted", "This device trusts the hub. Scan the login QR (or open the hub and enter the code).");
      proceed.textContent = "Open the hub";
      proceed.hidden = false;
      return;
    }
    show("untrusted", "Not trusted yet: check the fingerprint, then install the CA below.");
    recheck.hidden = false;
    openSteps();
  }

  proceed.addEventListener("click", go);
  recheck.addEventListener("click", check);
  openSteps();
  check();
})();

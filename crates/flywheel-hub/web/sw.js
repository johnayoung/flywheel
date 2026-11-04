// flywheel hub service worker: an offline app shell and notification
// clicks, nothing else.
//
// Never touched: the API, sockets, sign-in and sign-out, and anything not
// a same-origin GET. The network always comes first: a cached copy is
// used only when the hub cannot be reached, so auth is decided by the hub
// on every request that reaches it (a signed-out browser gets the sign-in
// page, never a cached app). Only the signed-in page (marked by the hub)
// and its /assets/ are kept, per build: a new hub binary starts a new
// cache.
"use strict";

const BUILD = "__FW_HUB_BUILD__";
const CACHE = `fw-hub-shell-${BUILD}`;
const SHELL_HEADER = "x-fw-hub-shell";

self.addEventListener("install", () => {
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil((async () => {
    for (const key of await caches.keys()) {
      if (key.startsWith("fw-hub-shell-") && key !== CACHE) await caches.delete(key);
    }
    await self.clients.claim();
  })());
});

function offlinePage() {
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="theme-color" content="#0c0d0c"><title>flywheel hub - offline</title>
<style>body{margin:0;min-height:100vh;display:grid;place-items:center;background:#0c0d0c;color:#ece6d6;
font:13px/1.6 ui-monospace,monospace;padding:env(safe-area-inset-top) 1.5rem 0}
b{color:#ff5a1f;letter-spacing:.2em;text-transform:uppercase;font-size:11px}
a{color:#ff5a1f}</style></head><body><main><b>flywheel hub</b>
<p>The hub cannot be reached. Your sessions keep running on it.</p>
<p><a href="/">Try again</a></p></main></body></html>`;
  return new Response(html, { status: 503, headers: { "Content-Type": "text/html; charset=utf-8" } });
}

async function navigate(request) {
  try {
    const response = await fetch(request);
    if (response.ok && response.headers.get(SHELL_HEADER) === "app") {
      const cache = await caches.open(CACHE);
      await cache.put("/", response.clone());
    }
    return response;
  } catch (_) {
    const cached = await caches.match("/", { cacheName: CACHE });
    return cached || offlinePage();
  }
}

async function asset(request) {
  try {
    const response = await fetch(request);
    if (response.ok) {
      const cache = await caches.open(CACHE);
      await cache.put(request, response.clone());
    }
    return response;
  } catch (error) {
    const cached = await caches.match(request, { cacheName: CACHE });
    if (cached) return cached;
    throw error;
  }
}

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  if (request.mode === "navigate" && url.pathname === "/" && !url.search) {
    event.respondWith(navigate(request));
  } else if (url.pathname.startsWith("/assets/")) {
    event.respondWith(asset(request));
  }
});

// A notification's click opens (or focuses) the hub on its session.
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const data = event.notification.data || {};
  const session = typeof data.session === "string" && /^[0-9a-f]+$/.test(data.session) ? data.session : null;
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    for (const client of windows) {
      if (new URL(client.url).origin !== self.location.origin) continue;
      await client.focus();
      client.postMessage({ type: "fw-hub-open", session });
      return;
    }
    await self.clients.openWindow(session ? `/#session=${session}` : "/");
  })());
});

// The page that loaded before this worker controlled it asks for the
// shell to be kept (the signed-in page and the /assets/ it uses); signing
// out empties the cache.
async function keepShell(urls) {
  const cache = await caches.open(CACHE);
  const page = await fetch("/", { credentials: "same-origin", cache: "no-store" });
  if (!page.ok || page.headers.get(SHELL_HEADER) !== "app") return;
  await cache.put("/", page);
  for (const raw of urls) {
    const url = new URL(raw, self.location.origin);
    if (url.origin !== self.location.origin || !url.pathname.startsWith("/assets/")) continue;
    // Kept already (this build's cache): not fetched again on every load.
    if (await cache.match(url)) continue;
    const response = await fetch(url, { credentials: "same-origin" });
    if (response.ok) await cache.put(url, response);
  }
}

self.addEventListener("message", (event) => {
  const data = event.data || {};
  if (data.type === "fw-hub-shell" && Array.isArray(data.urls)) {
    event.waitUntil(keepShell(data.urls.slice(0, 64)).catch(() => {}));
  } else if (data.type === "fw-hub-forget") {
    event.waitUntil((async () => {
      for (const key of await caches.keys()) {
        if (key.startsWith("fw-hub-shell-")) await caches.delete(key);
      }
    })());
  }
});

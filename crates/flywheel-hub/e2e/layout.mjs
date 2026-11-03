// End-to-end check of the hub's tabs + split panes in a real browser
// (opt-in; not part of scripts/check.sh, which has no node step).
//
//   node crates/flywheel-hub/e2e/layout.mjs [OUT_DIR]
//
// Env: FW (the fw binary; default target/debug/fw), PLAYWRIGHT (path to
// the playwright package; default: resolved from node's paths), CHROMIUM
// (a browser executable, when playwright's own is not installed), PORT (the
// hub's port; default: a free one). Runs a hub on a temp state dir and a
// port with a fake `claude` first on PATH;
// never touches the default state dir or port 7420. Screenshots go to
// OUT_DIR (default: a temp dir, printed). Exit 0 = every check passed.
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT || "playwright");

const root = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../../..");
const fw = process.env.FW || path.join(root, "target/debug/fw");
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "fw-hub-e2e-"));
const out = process.argv[2] || path.join(tmp, "shots");
fs.mkdirSync(out, { recursive: true });

const failures = [];
function check(ok, what) {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
    server.on("error", reject);
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// A fake Claude Code: prints a banner and a prompt, then echoes input.
const bin = path.join(tmp, "bin");
fs.mkdirSync(bin);
fs.writeFileSync(path.join(bin, "claude"), "#!/bin/sh\necho 'fake claude'\nprintf '> '\nexec cat\n", { mode: 0o755 });
const repo = path.join(tmp, "repo");
fs.mkdirSync(path.join(repo, ".git"), { recursive: true });
const state = path.join(tmp, "state");

const port = process.env.PORT ? Number(process.env.PORT) : await freePort();
const base = `http://127.0.0.1:${port}`;
const hub = spawn(fw, ["hub", "serve", "--state-dir", state, "--port", String(port), "--no-flywheel"], {
  env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, SHELL: "/bin/sh" },
  stdio: ["ignore", "ignore", "inherit"],
});

let browser;
try {
  for (let i = 0; !fs.existsSync(path.join(state, "server.json")); i++) {
    if (i > 300) throw new Error("hub did not start");
    await sleep(100);
  }
  const token = fs.readFileSync(path.join(state, "token"), "utf8").trim();
  const api = async (method, url, body) => {
    const response = await fetch(base + url, {
      method,
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await response.text();
    if (!response.ok) throw new Error(`${method} ${url}: ${response.status} ${text}`);
    return text ? JSON.parse(text) : null;
  };

  const ws = await api("POST", "/api/workspaces", { name: "e2e", folders: [repo] });
  const sids = [];
  for (const kind of ["claude", "shell", "shell"]) {
    const s = await api("POST", "/api/sessions", { workspace_id: ws.id, kind, cols: 100, rows: 30 });
    sids.push(s.id);
  }
  const pane = (id, session) => ({ type: "pane", id, session });
  const split = (dir, ratio, a, b) => ({ type: "split", dir, ratio, a, b });
  // Tab 1: claude | (shell / shell); tab 2: the claude again, alone.
  await api("PUT", `/api/workspaces/${ws.id}/layout`, {
    rev: 0,
    tabs: [
      { id: "t1", name: "agents", root: split("right", 0.5, pane("p1", sids[0]), split("down", 0.6, pane("p2", sids[1]), pane("p3", sids[2]))) },
      { id: "t2", name: "logs", root: pane("p4", sids[0]) },
    ],
  });

  // A second workspace with one shell, laid out on its own: jumping to it
  // and back must show its pane, never an empty one (the stage once kept
  // the other workspace's panes and skipped the rebuild).
  const repo2 = path.join(tmp, "repo2");
  fs.mkdirSync(path.join(repo2, ".git"), { recursive: true });
  const ws2 = await api("POST", "/api/workspaces", { name: "e2e-two", folders: [repo2] });
  const other = await api("POST", "/api/sessions", { workspace_id: ws2.id, kind: "shell", cols: 100, rows: 30 });
  await api("PUT", `/api/workspaces/${ws2.id}/layout`, { rev: 0, tabs: [{ id: "w1", name: "main", root: pane("q1", other.id) }] });

  browser = await chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});

  // --- desktop 1440x900 ---------------------------------------------------
  const desk = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await desk.newPage();
  await page.goto(`${base}/login?token=${token}`);
  await page.waitForURL(`${base}/`);
  // No #session: the last (only) workspace's layout comes back by itself.
  await page.waitForSelector("#terminal .pane .xterm", { timeout: 15000 });
  const stage = () => page.evaluate(() => {
    const host = document.querySelector("#terminal");
    const panes = [...host.querySelectorAll(".pane")];
    return {
      panes: panes.length,
      terms: host.querySelectorAll(".pane .xterm").length,
      tabs: document.querySelectorAll(".tab-list [role=tab]").length,
      floored: host.querySelectorAll(".pane-term.floored").length,
      empty: !document.querySelector("#empty").hidden,
    };
  });
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane .xterm").length === 3, null, { timeout: 15000 });
  let s = await stage();
  check(s.panes === 3 && s.terms === 3, `desktop: 3 live panes (${s.panes}/${s.terms})`);
  check(s.tabs === 2, `desktop: 2 tabs (${s.tabs})`);
  check(!s.empty, "desktop: no empty hero with a saved layout");
  check(s.floored === 0, "desktop: no pane held at the phone row floor");
  // The shells' prompts (top rows) are on screen, not clipped under the title.
  await page.waitForTimeout(800);
  const prompts = await page.evaluate(() => [...document.querySelectorAll("#terminal .pane-term")].map((host) => {
    const xt = host.querySelector(".xterm");
    const a = host.getBoundingClientRect();
    const b = xt.getBoundingClientRect();
    return b.top >= a.top - 1;
  }));
  check(prompts.every(Boolean), `desktop: every terminal's top row is inside its pane (${prompts})`);
  // Status dot rings stay their own size (they once covered whole panes).
  const ring = await page.evaluate(() => {
    const dots = [...document.querySelectorAll(".pane-head .dot")];
    return Math.max(0, ...dots.map((d) => parseFloat(getComputedStyle(d, "::after").width) || 0));
  });
  check(ring < 20, `desktop: status ring stays dot-sized (${ring}px)`);
  await page.screenshot({ path: path.join(out, "desktop-3-panes.png") });

  // A split never leaves a pane under 40 columns: p2 (a quarter of the
  // stage) halved would; the new shell gets its own tab instead.
  await page.locator('#terminal .pane[data-pane="p2"] .xterm').click();
  const tabsBefore = await page.evaluate(() => document.querySelectorAll(".tab-list [role=tab]").length);
  await page.keyboard.press("Alt+\\");
  await page.waitForTimeout(1500);
  let narrow = await page.evaluate(() => ({
    tabs: document.querySelectorAll(".tab-list [role=tab]").length,
    toast: [...document.querySelectorAll(".toast")].map((t) => t.textContent).join(" "),
  }));
  check(narrow.tabs === tabsBefore + 1 && /No room to split/.test(narrow.toast), `desktop: no split below the minimum pane size (${JSON.stringify(narrow)})`);
  // Drop that tab and its shell again (the checks below count sessions).
  const extra = await page.evaluate(() => /#session=([0-9a-f]+)/.exec(location.hash)?.[1]);
  await page.keyboard.press("Alt+x");
  await page.waitForTimeout(800);
  if (extra && !sids.includes(extra)) await api("DELETE", `/api/sessions/${extra}`);
  await page.locator(".tab-list [role=tab]", { hasText: "agents" }).click();
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane").length === 3);

  // Zoom one pane: the others are not "viewed" any more.
  await page.locator('#terminal .pane[data-pane="p1"] .xterm').click();
  await page.keyboard.press("Alt+z");
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane").length === 1);
  const parkedOnStage = await page.evaluate(() => document.querySelector("#pane-park").children.length);
  check(parkedOnStage === 2, `desktop: zoom parks the other 2 panes (${parkedOnStage})`);
  await page.keyboard.press("Alt+z");
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane").length === 3);

  // Jumps between the two workspaces (Alt 1-9 and the sidebar).
  const header = () => page.evaluate(() => ({
    crumb: document.querySelector("#crumb-session").textContent,
    ws: document.querySelector("#crumb-workspace").textContent,
    panes: document.querySelectorAll("#terminal .pane .xterm").length,
  }));
  const jumps = [];
  for (const key of ["Alt+4", "Alt+1", "Alt+4", "Alt+2", "Alt+4", "Alt+1"]) {
    await page.keyboard.press(key);
    await page.waitForTimeout(600);
    const h = await header();
    const want = key === "Alt+4" ? h.ws === "e2e-two" && h.panes === 1 : h.ws === "e2e" && h.panes === 3;
    if (!want || /Empty pane/.test(h.crumb)) jumps.push(`${key}: ${JSON.stringify(h)}`);
  }
  await page.locator(".session", { hasText: "sh 1" }).last().click();
  await page.waitForTimeout(600);
  const clicked = await header();
  if (clicked.ws !== "e2e-two" || clicked.panes !== 1) jumps.push(`sidebar: ${JSON.stringify(clicked)}`);
  check(jumps.length === 0, `desktop: jumping between workspaces shows their panes (${jumps.join("; ")})`);
  await page.keyboard.press("Alt+1");
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane .xterm").length === 3, null, { timeout: 5000 });

  // Reload restores the same layout.
  await page.reload();
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane .xterm").length === 3, null, { timeout: 15000 });
  s = await stage();
  check(s.panes === 3 && s.tabs === 2, `desktop: reload restores 3 panes + 2 tabs (${s.panes}/${s.tabs})`);
  // The second tab shows its one pane.
  await page.locator(".tab-list [role=tab]", { hasText: "logs" }).click();
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane").length === 1);
  check(true, "desktop: switching to tab 'logs' shows its 1 pane");
  await page.screenshot({ path: path.join(out, "desktop-tab-logs.png") });
  await page.locator(".tab-list [role=tab]", { hasText: "agents" }).click();
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane").length === 3);

  // A second, smaller browser: sh 2 is zoomed (and so sized) by the
  // first, so here it follows that bigger size, clipped. Its cursor (the
  // prompt, on a top row) stays in view, and narrow title bars keep names.
  await page.locator('#terminal .pane[data-pane="p2"] .xterm').click();
  await page.keyboard.press("Alt+z");
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane").length === 1);
  const small = await browser.newContext({ viewport: { width: 1100, height: 700 } });
  const sp = await small.newPage();
  await sp.goto(`${base}/login?token=${token}#session=${sids[0]}`);
  await sp.waitForFunction(() => document.querySelectorAll("#terminal .pane .xterm").length === 3, null, { timeout: 15000 });
  await sp.waitForFunction(() => !!document.querySelector('#terminal .pane[data-pane="p2"] .pane-term.following'), null, { timeout: 10000 })
    .then(() => check(true, "second browser: sh 2 follows the other viewer's size"), () => check(false, "second browser: sh 2 follows the other viewer's size"));
  await sp.waitForTimeout(500);
  const follow = await sp.evaluate(() => {
    const host = document.querySelector('#terminal .pane[data-pane="p2"] .pane-term');
    const cursor = host.querySelector(".xterm-helper-textarea").getBoundingClientRect();
    const box = host.getBoundingClientRect();
    return { inView: cursor.top >= box.top - 1 && cursor.top < box.bottom, cls: host.className };
  });
  check(follow.inView, `second browser: the following pane's cursor row is in view (${follow.cls})`);
  const heads = await sp.evaluate(() => [...document.querySelectorAll("#terminal .pane")].map((p) => {
    const name = p.querySelector(".pane-name");
    const tools = p.querySelector(".pane-tools");
    const box = p.getBoundingClientRect();
    return { name: name ? name.getBoundingClientRect().width : 0, toolsIn: tools ? tools.getBoundingClientRect().right <= box.right + 1 : true };
  }));
  check(heads.every((h) => h.name >= 20 && h.toolsIn), `second browser: title bars keep names and tools inside (${JSON.stringify(heads)})`);
  await sp.screenshot({ path: path.join(out, "desktop-second-viewer.png") });
  await small.close();
  await page.keyboard.press("Alt+z");
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane").length === 3);

  // A new tab offers the workspace's sessions instead of the generic hero.
  await page.keyboard.press("Alt+t");
  await page.waitForFunction(() => document.querySelectorAll(".tab-list [role=tab]").length === 3, null, { timeout: 5000 });
  await page.waitForFunction(() => document.querySelector("#empty-title").textContent === "Empty pane.", null, { timeout: 5000 })
    .then(() => check(true, "desktop: a new tab offers this workspace's sessions"), () => check(false, "desktop: a new tab offers this workspace's sessions"));
  const offered = await page.locator("#empty-actions button").count();
  check(offered === 4, `desktop: New shell + 3 sessions offered (${offered})`);
  await page.locator("#empty-actions button", { hasText: "sh 3" }).click();
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane .xterm").length === 1 && document.querySelector("#empty").hidden, null, { timeout: 10000 })
    .then(() => check(true, "desktop: picking one shows it in the new tab"), () => check(false, "desktop: picking one shows it in the new tab"));
  await page.screenshot({ path: path.join(out, "desktop-new-tab.png") });

  // Narrowing the window to phone width keeps the session on screen.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(500);
  const narrowed = await page.evaluate(() => ({
    empty: !document.querySelector("#empty").hidden,
    panes: document.querySelectorAll("#terminal .pane .xterm").length,
  }));
  check(!narrowed.empty && narrowed.panes === 1, `desktop -> phone width keeps a session on screen (${JSON.stringify(narrowed)})`);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(500);
  const widened = await page.evaluate(() => document.querySelectorAll("#terminal .pane .xterm").length);
  check(widened >= 1, `phone -> desktop width restores the layout (${widened} panes)`);
  await desk.close();

  // --- phone 390x844 ------------------------------------------------------
  const phone = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, deviceScaleFactor: 2 });
  const mp = await phone.newPage();
  // Without #session a phone lands on its start screen, not on a pane of
  // the saved layout (which would mark finished turns seen unasked).
  await mp.goto(`${base}/login?token=${token}`);
  await mp.waitForURL(`${base}/`);
  await mp.waitForTimeout(2500);
  const landing = await mp.evaluate(() => ({
    empty: !document.querySelector("#empty").hidden,
    panes: document.querySelectorAll("#terminal .pane").length,
  }));
  check(landing.empty && landing.panes === 0, `phone: lands on the start screen, no pane opened (${JSON.stringify(landing)})`);
  // Quick answers offer the prompt lowest on screen: an answered dialog
  // above a newer (y/n) question is not it.
  const quick = await mp.evaluate(() => window.FwHubPrompt.parse([
    "Do you want to run rm?", "❯ 1. Yes", "  2. Yes, and don't ask again", "  3. No (esc)", "",
    "Overwrite existing file config.toml? (y/n)",
  ]).options.map((o) => o.seq));
  check(quick[0] === "y\r", `phone: quick answers pick the newer y/n prompt (${JSON.stringify(quick)})`);
  await mp.goto(`${base}/login?token=${token}#session=${sids[1]}`);
  await mp.waitForSelector("#terminal .pane .xterm", { timeout: 15000 });
  await mp.waitForTimeout(800);
  const m = await mp.evaluate(() => ({
    panes: document.querySelectorAll("#terminal .pane").length,
    chips: [...document.querySelectorAll(".pane-chip")].map((c) => c.textContent),
    keys: document.querySelectorAll("#keybar .kb-key").length,
  }));
  check(m.panes === 1, `phone: one pane full screen (${m.panes})`);
  check(m.chips.length === 3, `phone: one chip per session, no duplicates (${JSON.stringify(m.chips)})`);
  check(m.keys > 5, `phone: key bar present (${m.keys} keys)`);
  // Every header button and key takes its own taps (nothing overlays them).
  const blocked = await mp.evaluate(() => {
    const targets = [
      ...document.querySelectorAll(".menu-toggle, #cmd-toggle, #keybar .kb-key, .pane-chip"),
    ].filter((n) => n.offsetParent !== null);
    const bad = [];
    for (const node of targets) {
      const r = node.getBoundingClientRect();
      const x = r.left + r.width / 2;
      const y = r.top + r.height / 2;
      if (x < 0 || y < 0 || x > innerWidth || y > innerHeight) continue;
      // A key scrolled out of its row's view (the row scrolls sideways) is
      // not covered, just not on screen yet.
      const row = node.closest(".kb-keys");
      if (row) {
        const box = row.getBoundingClientRect();
        if (x < box.left || x > box.right) continue;
      }
      const hit = document.elementFromPoint(x, y);
      // A key scrolled under the bar's pinned keys (kbd, compose) is fine.
      const ok = hit && (node.contains(hit) || (node.matches(".kb-key") && hit.closest(".kb-key") && !hit.closest(".dot")));
      if (!ok) bad.push(`${node.className}:${node.textContent.trim().slice(0, 12)} under ${hit ? hit.tagName + "." + hit.className : "nothing"} @${Math.round(x)},${Math.round(y)}`);
    }
    return bad;
  });
  check(blocked.length === 0, `phone: no control is covered (${blocked.join(", ")})`);
  // ... and every one is at least 44px tall (key bar keys share the width).
  const undersized = await mp.evaluate(() => [
    ...document.querySelectorAll(".menu-toggle, #cmd-toggle, #flip-toggle, #fw-toggle, #keybar .kb-primary .kb-key, .pane-chip"),
  ].filter((n) => n.offsetParent !== null)
    .map((n) => [n.className.split(" ")[0] || n.id, Math.round(n.getBoundingClientRect().height)])
    .filter(([, h]) => h < 44));
  check(undersized.length === 0, `phone: tap targets at least 44px tall (${JSON.stringify(undersized)})`);
  // Icon-only and second-row controls are 44px wide too (the first key row
  // shares the width so every key stays in view).
  const narrowTargets = await mp.evaluate(() => [
    ...document.querySelectorAll(".menu-toggle, #cmd-toggle, #flip-toggle, #rv-toggle, #keybar .kb-second .kb-key, .qa, .qa-esc"),
  ].filter((n) => n.offsetParent !== null)
    .map((n) => [n.id || n.className.split(" ")[0], Math.round(n.getBoundingClientRect().width), Math.round(n.getBoundingClientRect().height)])
    .filter(([, w, h]) => w < 44 || h < 44));
  check(narrowTargets.length === 0, `phone: icon and second-row targets at least 44x44 (${JSON.stringify(narrowTargets)})`);
  // The header with a usage meter and a status pill (as an agent with
  // spend shows): the name, pill and why? keep their room, nothing is
  // covered or pushed off the screen.
  await mp.evaluate(() => {
    const set = (sel, text) => { const n = document.querySelector(sel); n.hidden = false; if (text) n.textContent = text; };
    set("#meter-usage", "65.0k tok · $0.51 · sonnet-5-5");
    set("#crumb-status", "blocked");
    document.querySelector("#crumb-status").className = "agent-pill blocked";
    set("#crumb-why");
  });
  await mp.waitForTimeout(300);
  const headerHits = await mp.evaluate(() => {
    const bad = [];
    for (const sel of ["#crumb-session", "#crumb-status", "#crumb-why", ".menu-toggle", "#cmd-toggle"]) {
      const node = document.querySelector(sel);
      if (!node || node.offsetParent === null) continue;
      const r = node.getBoundingClientRect();
      for (const x of [r.left + 2, r.left + r.width / 2, r.right - 2]) {
        const hit = document.elementFromPoint(x, r.top + r.height / 2);
        if (!hit || !(node === hit || node.contains(hit))) bad.push(`${sel} @${Math.round(x)} under ${hit ? hit.id || hit.className : "nothing"}`);
      }
      if (r.right > innerWidth + 1) bad.push(`${sel} off screen`);
    }
    return bad;
  });
  check(headerHits.length === 0, `phone: header with usage keeps name, status and why? whole (${headerHits.join(", ")})`);
  // Every pill showing at once (share, rec on, flip, suggested, review,
  // flywheel) at 390 and 430: they wrap under the name rather than paint
  // over it, and the name keeps readable room.
  await mp.evaluate(() => {
    for (const id of ["share-toggle", "rec-toggle", "flip-toggle", "rv-toggle", "fw-toggle"]) document.getElementById(id).hidden = false;
    document.getElementById("rec-label").textContent = "rec 0:42";
  });
  for (const width of [390, 430]) {
    await mp.setViewportSize({ width, height: 844 });
    await mp.waitForTimeout(300);
    const crowded = await mp.evaluate(() => {
      // A render may have hidden them again (this is a shell session).
      for (const id of ["share-toggle", "rec-toggle", "flip-toggle", "rv-toggle", "fw-toggle", "crumb-status", "crumb-why"]) document.getElementById(id).hidden = false;
      const bad = [];
      for (const sel of ["#crumb-session", "#crumb-status", "#crumb-why"]) {
        const node = document.querySelector(sel);
        const r = node.getBoundingClientRect();
        const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        if (!hit || !(node === hit || node.contains(hit))) bad.push(`${sel} under ${hit ? hit.id || hit.className : "nothing"}`);
        if (r.right > innerWidth + 1) bad.push(`${sel} off screen`);
      }
      const name = document.querySelector("#crumb-session").getBoundingClientRect().width;
      if (name < 100) bad.push(`name only ${Math.round(name)}px`);
      return bad;
    });
    check(crowded.length === 0, `phone ${width}: every header pill shown, the name, status and why? stay whole (${crowded.join(", ")})`);
  }
  await mp.setViewportSize({ width: 390, height: 844 });
  await mp.screenshot({ path: path.join(out, "phone-solo.png") });
  // The menu opens the sidebar.
  await mp.locator(".menu-toggle").tap();
  await mp.waitForFunction(() => document.body.classList.contains("side-open"), null, { timeout: 5000 }).then(
    () => check(true, "phone: menu opens the sessions sidebar"),
    () => check(false, "phone: menu opens the sessions sidebar"),
  );
  await mp.screenshot({ path: path.join(out, "phone-sidebar.png") });
  await phone.close();
} finally {
  if (browser) await browser.close();
  hub.kill("SIGTERM");
  await new Promise((r) => hub.once("exit", r));
}

console.log(`screenshots: ${out}`);
if (failures.length) {
  console.log(`${failures.length} check(s) failed`);
  process.exit(1);
}
fs.rmSync(path.join(tmp, "state"), { recursive: true, force: true });

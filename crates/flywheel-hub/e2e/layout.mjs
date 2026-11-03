// End-to-end check of the hub's tabs + split panes in a real browser
// (opt-in; not part of scripts/check.sh, which has no node step).
//
//   node crates/flywheel-hub/e2e/layout.mjs [OUT_DIR]
//
// Env: FW (the fw binary; default target/debug/fw), PLAYWRIGHT (path to
// the playwright package; default: resolved from node's paths), CHROMIUM
// (a browser executable, when playwright's own is not installed). Runs a hub
// on a temp state dir and a free port with a fake `claude` first on PATH;
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

const port = await freePort();
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

  // Zoom one pane: the others are not "viewed" any more.
  await page.locator('#terminal .pane[data-pane="p1"] .xterm').click();
  await page.keyboard.press("Alt+z");
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane").length === 1);
  const parkedOnStage = await page.evaluate(() => document.querySelector("#pane-park").children.length);
  check(parkedOnStage === 2, `desktop: zoom parks the other 2 panes (${parkedOnStage})`);
  await page.keyboard.press("Alt+z");
  await page.waitForFunction(() => document.querySelectorAll("#terminal .pane").length === 3);

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
      const hit = document.elementFromPoint(x, y);
      // A key scrolled under the bar's pinned keys (kbd, compose) is fine.
      const ok = hit && (node.contains(hit) || (node.matches(".kb-key") && hit.closest(".kb-key") && !hit.closest(".dot")));
      if (!ok) bad.push(`${node.className}:${node.textContent.trim().slice(0, 12)} under ${hit ? hit.tagName + "." + hit.className : "nothing"} @${Math.round(x)},${Math.round(y)}`);
    }
    return bad;
  });
  check(blocked.length === 0, `phone: no control is covered (${blocked.join(", ")})`);
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

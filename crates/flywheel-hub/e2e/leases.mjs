// End-to-end check of a stuck lease in a real browser: a lease held past
// its stuck threshold while a session waits is a "lease R stuck" row in
// Needs you (holder, its process tree, who waits) and STUCK in the
// waiting session's inspector. Release asks first (cancel keeps it),
// Keep waiting puts it away for another threshold, and Release confirmed
// breaks the lease, ends the holder's process group, and grants the
// waiter. hub.toml's [leases] stuck_after makes the threshold seconds.
// Opt-in, like layout.mjs:
//
//   node crates/flywheel-hub/e2e/leases.mjs [OUT_DIR]
//
// Env: FW (default target/debug/fw), PLAYWRIGHT, CHROMIUM, PORT (as
// layout.mjs). Runs a hub on a temp state dir with a fake `claude` first
// on PATH; never touches the default state dir or port 7420.
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
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "fw-hub-leases-e2e-"));
const out = process.argv[2] || path.join(tmp, "shots");
fs.mkdirSync(out, { recursive: true });

const failures = [];
function check(ok, what) {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
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
const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

const bin = path.join(tmp, "bin");
fs.mkdirSync(bin);
const claudeConfig = path.join(tmp, "claude-config");
fs.mkdirSync(claudeConfig);
fs.writeFileSync(path.join(bin, "claude"), "#!/bin/sh\necho 'fake claude'\nprintf '> '\nexec cat >/dev/null\n", { mode: 0o755 });
const repo = path.join(tmp, "repo");
fs.mkdirSync(path.join(repo, ".git"), { recursive: true });
const state = path.join(tmp, "state");
fs.mkdirSync(state, { recursive: true, mode: 0o700 });
// Stuck after 3 s with no history (a real hub: 1 h).
fs.writeFileSync(path.join(state, "hub.toml"), '[leases]\nstuck_margin = "2s"\nstuck_after = "3s"\n');
const port = process.env.PORT ? Number(process.env.PORT) : await freePort();
const base = `http://127.0.0.1:${port}`;
const hub = spawn(fw, ["hub", "serve", "--state-dir", state, "--port", String(port), "--no-flywheel"], {
  env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, SHELL: "/bin/sh", CLAUDE_CONFIG_DIR: claudeConfig, FW_BIN: fw },
  stdio: ["ignore", "ignore", "inherit"],
});
// The holder: a suite runner with a child, in its own process group.
const holder = spawn("sh", ["-c", "sleep 300 & wait"], { detached: true, stdio: "ignore" });
const waiterProc = spawn("sleep", ["300"], { detached: true, stdio: "ignore" });

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
  const host = os.hostname();
  const ws = await api("POST", "/api/workspaces", { name: "e2e", folders: [repo] });
  const waiter = await api("POST", "/api/sessions", { workspace_id: ws.id, kind: "claude", name: "gate-run", cols: 100, rows: 30 });
  const held = await api("POST", "/api/leases", { resource: "cpu:test-suite", ttl_secs: 600, holder: { host, pid: holder.pid, label: "scripts/check.sh" } });
  check(held.state === "granted", `the holder has the lease (${held.state})`);
  const queued = await api("POST", "/api/leases", {
    resource: "cpu:test-suite", ttl_secs: 600, wait_secs: 600, block_secs: 0,
    holder: { host, pid: waiterProc.pid, label: "scripts/check.sh", session: waiter.id },
  });
  check(queued.state === "waiting", `gate-run waits in line (${queued.state})`);

  browser = await chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
  const desk = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await desk.newPage();
  await page.goto(`${base}/login?token=${token}`);
  await page.waitForURL(`${base}/`);
  const row = () => page.locator('#attention li[data-hold^="lease:"]');
  await page.waitForSelector('#attention li[data-hold^="lease:"]', { timeout: 30000 }).catch(() => {});
  const shown = await page.evaluate(() => {
    const li = document.querySelector('#attention li[data-hold^="lease:"]');
    if (!li) return null;
    return {
      name: li.querySelector(".attn-name").textContent,
      detail: li.querySelector(".attn-detail").textContent,
      kind: li.querySelector(".attn-status").textContent,
      question: (li.querySelector(".attn-handoff-preview") || {}).textContent || "",
      buttons: [...li.querySelectorAll(".attn-handoff-actions button")].map((b) => b.textContent),
      count: document.querySelector("#attention .attn-count")?.textContent || "",
    };
  });
  check(shown && shown.name === "lease cpu:test-suite stuck" && shown.kind === "lease", `a stuck-lease row in Needs you (${JSON.stringify(shown && [shown.name, shown.kind])})`);
  check(shown && shown.detail === "gate-run waits", `it names who waits (${shown && shown.detail})`);
  check(shown && shown.question.includes("scripts/check.sh") && shown.question.includes("sleep 300"), `it says who holds it and what its process runs (${shown && shown.question})`);
  check(shown && shown.buttons.join(",") === "Keep waiting,Release", `Keep waiting and Release (${shown && shown.buttons})`);
  check(shown && /1 lease stuck/.test(shown.count), `the heading counts it (${shown && shown.count})`);
  await page.screenshot({ path: path.join(out, "desktop-lease-stuck.png") });

  const calm = await page.evaluate(() => [...document.querySelectorAll(".list-calm")].some((n) => n.offsetParent && /Nothing needs you/.test(n.textContent)));
  check(!calm, "the list does not also say nothing needs you");
  // The inspector of the waiting session says STUCK (it rests: unfold).
  const fold = page.locator(".list-group.fold:not(.open)");
  if (await fold.count()) await fold.first().click();
  await page.locator(`.session[data-id="${waiter.id}"] .session-name`).hover();
  await page.waitForSelector("#inspector:not([hidden])", { timeout: 5000 }).catch(() => {});
  const insp = await page.evaluate(() => document.querySelector("#inspector")?.textContent || "");
  check(/STUCK cpu:test-suite: held \d+m by scripts\/check\.sh/.test(insp), `inspector: STUCK with the holder (${insp.match(/STUCK[^,]*/)?.[0]})`);
  await page.screenshot({ path: path.join(out, "desktop-lease-inspector.png") });
  await page.keyboard.press("Escape");
  await page.mouse.move(1400, 880);

  // Release asks first; cancelled, nothing changes.
  await row().locator("button", { hasText: "Release" }).click();
  await page.waitForSelector("#dlg-confirm[open]", { timeout: 5000 }).catch(() => {});
  const title = await page.evaluate(() => document.querySelector("#dlg-confirm[open] #confirm-title")?.textContent || "");
  check(title === "Release cpu:test-suite?", `Release asks to confirm (${title})`);
  await page.screenshot({ path: path.join(out, "desktop-lease-confirm.png") });
  await page.locator('#dlg-confirm button[value="cancel"]').click();
  await sleep(500);
  let leases = await api("GET", "/api/leases?resource=cpu:test-suite");
  check(leases.leases.length === 1 && alive(holder.pid), "cancelled: the lease and its holder stay");

  // Keep waiting: away until another threshold passes, then back.
  await row().locator("button", { hasText: "Keep waiting" }).click();
  await page.waitForFunction(() => !document.querySelector('#attention li[data-hold^="lease:"]'), null, { timeout: 5000 }).catch(() => {});
  check((await row().count()) === 0, "Keep waiting: the row goes");
  await page.waitForSelector('#attention li[data-hold^="lease:"]', { timeout: 30000 }).catch(() => {});
  check((await row().count()) === 1, "past another threshold: the row is back");

  // Release, confirmed.
  await row().locator("button", { hasText: "Release" }).click();
  await page.waitForSelector("#dlg-confirm[open]", { timeout: 5000 }).catch(() => {});
  await page.locator("#confirm-ok").click();
  await page.waitForFunction(() => !document.querySelector('#attention li[data-hold^="lease:"]'), null, { timeout: 10000 }).catch(() => {});
  check((await row().count()) === 0, "released: the row goes");
  for (let i = 0; i < 50 && alive(holder.pid); i++) await sleep(100);
  check(!alive(holder.pid), "released: the holder's process group ended");
  leases = await api("GET", "/api/leases?resource=cpu:test-suite");
  check(leases.leases.length === 1 && leases.leases[0].holder.session === waiter.id, `released: gate-run holds it now (${JSON.stringify(leases.leases.map((l) => l.holder.label))})`);
  const kept = (await api("GET", "/api/decisions?kind=awaiting_approval")).decisions;
  check(kept.map((d) => d.answer).sort().join(",") === "keep waiting,release", `decision record: keep waiting, then release (${kept.map((d) => d.answer)})`);
  await page.screenshot({ path: path.join(out, "desktop-lease-released.png") });
} finally {
  if (browser) await browser.close();
  for (const p of [holder, waiterProc]) {
    try {
      process.kill(-p.pid, "SIGKILL");
    } catch {}
  }
  hub.kill("SIGTERM");
  await new Promise((r) => hub.once("exit", r));
}

console.log(`screenshots: ${out}`);
if (failures.length) {
  console.log(`${failures.length} check(s) failed`);
  process.exit(1);
}
fs.rmSync(state, { recursive: true, force: true });

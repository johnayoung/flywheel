// End-to-end check of the close dialog in a real browser: closing a
// session in its own worktree shows the close checklist (not merged, its
// target/ size, its untracked files), offers remove and clean only when
// they lose nothing, and runs them as `fw hub close` does (the worktree
// goes, the unmerged branch stays, the outcome is on its end record).
// An agent that is not at rest is not closed: the dialog says why and
// offers the old kill instead. Opened from the pane menu and from the
// inspector.
// Opt-in, like layout.mjs:
//
//   node crates/flywheel-hub/e2e/close.mjs [OUT_DIR]
//
// Env: FW (default target/debug/fw), PLAYWRIGHT, CHROMIUM, PORT (as
// layout.mjs). Runs a hub on a temp state dir with a fake `claude` first
// on PATH; never touches the default state dir or port 7420.
import { execFileSync, spawn } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT || "playwright");

const root = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../../..");
const fw = process.env.FW || path.join(root, "target/debug/fw");
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "fw-hub-close-e2e-"));
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
const git = (dir, ...args) =>
  execFileSync("git", ["-C", dir, "-c", "user.name=e2e", "-c", "user.email=e2e@e2e", "-c", "commit.gpgsign=false", ...args], { encoding: "utf8" }).trim();

const bin = path.join(tmp, "bin");
fs.mkdirSync(bin);
const claudeConfig = path.join(tmp, "claude-config");
fs.mkdirSync(claudeConfig);
fs.writeFileSync(path.join(bin, "claude"), "#!/bin/sh\necho 'fake claude'\nprintf '> '\nexec cat >/dev/null\n", { mode: 0o755 });
const repo = path.join(tmp, "repo");
fs.mkdirSync(repo);
git(repo, "init", "-q", "-b", "main");
fs.writeFileSync(path.join(repo, "Cargo.toml"), "[package]\n");
fs.writeFileSync(path.join(repo, ".gitignore"), "target/\n");
git(repo, "add", ".");
git(repo, "commit", "-q", "-m", "init");
const state = path.join(tmp, "state");
const port = process.env.PORT ? Number(process.env.PORT) : await freePort();
const base = `http://127.0.0.1:${port}`;
const hub = spawn(fw, ["hub", "serve", "--state-dir", state, "--port", String(port), "--no-flywheel"], {
  env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, SHELL: "/bin/sh", CLAUDE_CONFIG_DIR: claudeConfig, FW_BIN: fw },
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
  // A shell in a worktree of its own, with a commit main lacks and
  // build output; it exits at once.
  const done = await api("POST", "/api/sessions", {
    workspace_id: ws.id, kind: "shell", name: "worker", command: ["sh", "-c", "sleep 1"],
    worktree: { task: "feat-x" }, cols: 100, rows: 30,
  });
  const wt = done.worktree.path;
  fs.writeFileSync(path.join(wt, "x"), "x");
  git(wt, "add", "x");
  git(wt, "commit", "-q", "-m", "x");
  fs.mkdirSync(path.join(wt, "target/debug"), { recursive: true });
  fs.writeFileSync(path.join(wt, "target/debug/blob"), Buffer.alloc(3 * 1024 * 1024));
  // An agent that never reports a status: not at rest.
  const busy = await api("POST", "/api/sessions", { workspace_id: ws.id, kind: "claude", name: "busy", worktree: { task: "feat-y" }, cols: 100, rows: 30 });
  fs.writeFileSync(path.join(busy.worktree.path, "y"), "y");
  git(busy.worktree.path, "add", "y");
  git(busy.worktree.path, "commit", "-q", "-m", "y");
  fs.writeFileSync(path.join(busy.worktree.path, "notes.md"), "unsaved");
  for (let i = 0; (await api("GET", `/api/sessions/${done.id}`)).state !== "exited"; i++) {
    if (i > 100) throw new Error("worker never exited");
    await sleep(100);
  }

  browser = await chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
  const desk = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await desk.newPage();
  await page.goto(`${base}/login?token=${token}`);
  await page.waitForURL(`${base}/`);
  await page.waitForFunction(() => typeof closeSession === "function" && state.sessions.length >= 2, null, { timeout: 15000 });

  // From the pane menu: open the worker, right-click its pane, pick close.
  await page.evaluate((id) => openSession(id), done.id);
  await page.waitForSelector(".pane", { timeout: 10000 });
  const box = await page.locator(".pane").first().boundingBox();
  await page.evaluate(({ id, x, y }) => {
    const pane = L.panes(activeTab().root).find((p) => p.session === id);
    paneMenu({ id: pane.id, sessionId: id }, x, y);
  }, { id: done.id, x: box.x + 50, y: box.y + 50 });
  const item = page.locator("#ctx-menu").getByText("Close session worker");
  check(await item.count() === 1, "the pane menu offers Close session");
  await item.click();
  await page.waitForSelector("#dlg-close[open]", { timeout: 5000 });
  await page.waitForFunction(() => !document.querySelector("#cl-ok").disabled, null, { timeout: 15000 });
  const shown = await page.evaluate(() => ({
    rows: [...document.querySelectorAll("#cl-list li")].map((li) => li.textContent),
    removeOk: !document.querySelector("#cl-wt-remove").disabled,
    cleanOk: !document.querySelector("#cl-build-clean").disabled,
    why: document.querySelector("#cl-wt-why").textContent,
  }));
  check(shown.rows.some((r) => /not merged: 1 commit\(s\) main lacks/.test(r)), `the checklist says not merged (${shown.rows})`);
  check(shown.rows.some((r) => /build output target\/: 3 MiB/.test(r)), `the checklist sizes target/ (${shown.rows})`);
  check(shown.removeOk && shown.cleanOk, "remove and clean are offered (nothing lost)");
  check(/branch is kept/.test(shown.why), `it says the branch is kept (${shown.why})`);
  await page.screenshot({ path: path.join(out, "close-checklist.png") });
  await page.check("#cl-wt-remove");
  await page.check("#cl-build-clean");
  await page.uncheck("#cl-forget");
  await page.click("#cl-ok");
  await page.waitForSelector("#dlg-close", { state: "hidden", timeout: 15000 });
  const after = await api("GET", `/api/sessions/${done.id}`);
  const close = after.ended && after.ended.close;
  check(close && close.worktree.outcome === "done" && close.build.outcome === "done", `the close ran as fw hub close does (${JSON.stringify(close && [close.worktree, close.build])})`);
  check(!fs.existsSync(wt), "the worktree is gone");
  check(/feat-x/.test(git(repo, "branch", "--list", "*feat-x")), "its unmerged branch is kept");
  check(after.id === done.id, "kept in the list when asked to (its outcome on its record)");

  // From the inspector: an agent not at rest. Its row says its branch's
  // review coverage once a review is recorded.
  await api("POST", "/api/verdicts", { repo: busy.worktree.path, base: "main", head: "feat-y", verdict: "READY" })
    .catch(async () => api("POST", "/api/verdicts", { repo: busy.worktree.path, base: "main", head: git(busy.worktree.path, "rev-parse", "HEAD"), verdict: "READY" }));
  await page.evaluate((id) => inspector.toggle(id), busy.id);
  await page.waitForFunction(() => /all 1 commits READY/.test(document.querySelector("#inspector [data-wk=reviews]")?.textContent || ""), null, { timeout: 15000 })
    .then(() => check(true, "the inspector shows review coverage of its branch"))
    .catch(async () => check(false, `the inspector shows review coverage (${await page.locator("#inspector").textContent()})`));
  const inspectClose = page.locator(".inspector").getByRole("button", { name: "Close session" });
  check(await inspectClose.count() >= 1, "the inspector offers Close session");
  await inspectClose.first().click();
  await page.waitForSelector("#dlg-close[open]", { timeout: 5000 });
  await page.waitForFunction(() => !document.querySelector("#cl-ok").disabled, null, { timeout: 15000 });
  const busyShown = await page.evaluate(() => ({
    removeOff: document.querySelector("#cl-wt-remove").disabled,
    why: document.querySelector("#cl-wt-why").textContent,
  }));
  check(busyShown.removeOff && /untracked/.test(busyShown.why), `remove is refused over untracked work (${busyShown.why})`);
  await page.click("#cl-ok");
  await page.waitForFunction(() => !document.querySelector("#cl-kill").hidden, null, { timeout: 15000 });
  const said = await page.locator("#cl-state").textContent();
  check(/not at rest/.test(said), `a busy agent is not closed mid-flight (${said})`);
  await page.screenshot({ path: path.join(out, "close-busy.png") });
  await page.click("#dlg-close button[value=cancel]");
  check((await api("GET", `/api/sessions/${busy.id}`)).state === "running", "cancelled: it still runs");
  check(fs.existsSync(path.join(busy.worktree.path, "notes.md")), "its untracked work stays");

  // A worktree made with plain git: listed as not tracked, adopted from
  // the list, then closed by the same dialog.
  const plain = path.join(tmp, "repo.worktrees", "plain-work");
  git(repo, "worktree", "add", "-q", "-b", "plain-work", plain, "main");
  await page.evaluate((id) => pickScope(id), ws.id);
  await page.waitForSelector(".ws-worktrees", { timeout: 70000 });
  const folded = await page.evaluate(() => ({
    line: document.querySelector(".ws-worktrees").textContent,
    rows: document.querySelectorAll(".ws-change").length,
    height: document.querySelector(".ws-worktrees").getBoundingClientRect().height,
  }));
  check(/1 not tracked/.test(folded.line) && folded.rows === 0 && folded.height < 40, `one quiet folded line, no row per worktree (${JSON.stringify(folded)})`);
  await page.click(".ws-worktrees");
  await page.locator(".ws-change button", { hasText: "adopt" }).click();
  await page.waitForSelector(".ws-change-list .ws-change[data-change]", { timeout: 15000 });
  check(await page.locator(".ws-worktrees").getAttribute("aria-expanded") === "true", "it stays open, the worktree now adopted");
  const adopted = await api("GET", "/api/changes");
  check(adopted.changes.length === 1 && adopted.changes[0].worktree.branch === "plain-work", "adopted from the list");
  check(adopted.changes[0].base_from === "reflog", `its base came from its reflog (${adopted.changes[0].base_from})`);
  await page.screenshot({ path: path.join(out, "adopted.png") });
  // Close reads its dry-run checklist: untracked work refuses remove.
  fs.writeFileSync(path.join(plain, "scratch.md"), "keep me");
  await page.locator(".ws-change[data-change] button", { hasText: "close" }).click();
  await page.waitForSelector("#dlg-close[open]", { timeout: 5000 });
  await page.waitForFunction(() => !document.querySelector("#cl-ok").disabled, null, { timeout: 15000 });
  check(await page.locator("#cl-wt-remove").isDisabled(), "remove is refused while the adopted worktree holds untracked work");
  await page.click("#dlg-close button[value=cancel]");
  fs.rmSync(path.join(plain, "scratch.md"));
  await page.locator(".ws-change[data-change] button", { hasText: "close" }).click();
  await page.waitForSelector("#dlg-close[open]", { timeout: 5000 });
  await page.waitForFunction(() => !document.querySelector("#cl-ok").disabled, null, { timeout: 15000 });
  check(await page.locator("#cl-forget").isHidden(), "a change has no list entry to forget");
  await page.check("#cl-wt-remove");
  await page.click("#cl-ok");
  await page.waitForSelector("#dlg-close", { state: "hidden", timeout: 15000 });
  check(!fs.existsSync(plain), "the adopted worktree is removed (clean, nothing lost)");
  check((await api("GET", "/api/changes")).changes.length === 0, "and no longer tracked");
} catch (error) {
  console.error(error);
  failures.push(String(error));
} finally {
  if (browser) await browser.close();
  hub.kill("SIGTERM");
}
console.log(failures.length ? `${failures.length} FAILED` : "all ok");
process.exit(failures.length ? 1 : 0);

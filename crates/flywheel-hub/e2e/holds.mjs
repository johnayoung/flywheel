// End-to-end check of Needs you's hold groups in a real browser: three
// start requests from one lead show as one group (their own rows step
// aside), one member is left out, "Approve all" starts the others as one
// decision (one batch in the decision record), and the one left out
// stays, now a row of its own. Folded in the sidebar until tapped; on
// the phone's start screen open, and it fits the screen.
// Opt-in, like layout.mjs:
//
//   node crates/flywheel-hub/e2e/holds.mjs [OUT_DIR]
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
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "fw-hub-holds-e2e-"));
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

const bin = path.join(tmp, "bin");
fs.mkdirSync(bin);
const claudeConfig = path.join(tmp, "claude-config");
fs.mkdirSync(claudeConfig);
// A claude that reads lines forever.
fs.writeFileSync(path.join(bin, "claude"), "#!/bin/sh\necho 'fake claude'\nprintf '> '\nexec cat >/dev/null\n", { mode: 0o755 });
const repo = path.join(tmp, "repo");
fs.mkdirSync(path.join(repo, ".git"), { recursive: true });
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
  const lead = await api("POST", "/api/sessions", { workspace_id: ws.id, kind: "claude", name: "lead", cols: 100, rows: 30 });
  await api("PUT", `/api/sessions/${lead.id}/start-policy`, { policy: "ask" });
  for (const n of [1, 2, 3]) {
    const waiting = await api("POST", "/api/sessions", { workspace_id: ws.id, kind: "claude", name: `w${n}`, prompt: `task ${n}`, opened_by: lead.id, cols: 100, rows: 30 });
    check(!!waiting.waiting, `start ${n} waits on the operator`);
  }

  browser = await chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
  const desk = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await desk.newPage();
  await page.goto(`${base}/login?token=${token}`);
  await page.waitForURL(`${base}/`);
  await page.waitForSelector("#attention .attn-group", { timeout: 15000 }).catch(() => {});
  const shape = () =>
    page.evaluate(() => {
      const group = document.querySelector("#attention .attn-group");
      return {
        groups: document.querySelectorAll("#attention .attn-group").length,
        cause: group ? group.querySelector(".attn-name").textContent : null,
        kind: group ? group.querySelector(".attn-status").textContent : null,
        members: group ? group.querySelectorAll(".attn-members li").length : 0,
        startRows: document.querySelectorAll("#attention li[data-start]").length,
        count: document.querySelector("#attention .attn-count")?.textContent || "",
        buttons: group ? [...group.querySelectorAll(".attn-group-answer")].map((b) => b.textContent) : [],
      };
    });
  // Folded in the sidebar: one line naming who; tapped, it opens.
  const folded = await page.evaluate(() => {
    const group = document.querySelector("#attention .attn-group");
    return group ? { members: group.querySelectorAll(".attn-members li").length, detail: group.querySelector(".attn-detail").textContent, height: group.getBoundingClientRect().height } : null;
  });
  check(folded && folded.members === 0 && folded.detail === "w1, w2, w3" && folded.height < 60, `desktop: the group is one folded row naming who (${JSON.stringify(folded)})`);
  await page.locator("#attention .attn-row.group").click();
  let s = await shape();
  check(s.groups === 1 && s.members === 3, `desktop: one group of 3 (${JSON.stringify(s)})`);
  check(s.cause === "start_session from lead" && s.kind === "approval", `desktop: the group names its cause and kind (${s.cause}, ${s.kind})`);
  check(s.startRows === 0, `desktop: grouped starts step aside (${s.startRows} rows)`);
  check(/3 in 1 group/.test(s.count), `desktop: the count says grouped (${s.count})`);
  check(s.buttons.join(",") === "Decline all,Approve all", `desktop: approve and decline all (${s.buttons})`);
  await page.screenshot({ path: path.join(out, "desktop-hold-group.png") });

  // Leave w3 out, answer the rest at once.
  await page.locator("#attention .attn-members li", { hasText: "w3" }).locator("input[type=checkbox]").uncheck();
  check(await page.locator("#attention .attn-members li.out").count() === 1, "desktop: a member left out is struck through");
  await page.locator("#attention .attn-group-answer", { hasText: "Approve all" }).click();
  await page.waitForFunction(() => !document.querySelector("#attention .attn-group") && document.querySelectorAll("#attention li[data-start]").length === 1, null, { timeout: 15000 }).catch(() => {});
  s = await shape();
  check(s.groups === 0 && s.startRows === 1, `desktop: answered; the one left out is its own row (${JSON.stringify(s)})`);
  const listed = (await api("GET", "/api/sessions")).sessions.map((x) => x.name);
  check(listed.includes("w1") && listed.includes("w2") && !listed.includes("w3"), `w1 and w2 started, w3 not (${listed})`);
  const kept = (await api("GET", "/api/decisions?kind=awaiting_approval")).decisions;
  check(kept.length === 2 && kept.every((d) => d.answer === "approved" && d.batch && d.batch === kept[0].batch && d.by === "operator"),
    `decision record: 2 approved in one batch (${JSON.stringify(kept.map((d) => [d.answer, d.batch, d.by]))})`);
  await page.screenshot({ path: path.join(out, "desktop-hold-group-answered.png") });

  // Two more: a group again; on a phone it fits the screen.
  for (const n of [4, 5]) {
    await api("POST", "/api/sessions", { workspace_id: ws.id, kind: "claude", name: `w${n}`, prompt: `task ${n}`, opened_by: lead.id, cols: 100, rows: 30 });
  }
  const phone = await browser.newContext({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });
  const pp = await phone.newPage();
  await pp.goto(`${base}/login?token=${token}`);
  await pp.waitForURL(`${base}/`);
  await pp.waitForSelector("#empty-attention .attn-group, #attention .attn-group", { timeout: 15000 }).catch(() => {});
  const fit = await pp.evaluate(() => {
    // The start screen's list (the drawer's copy is off-screen).
    const group = [...document.querySelectorAll(".attn-group")].find((g) => {
      const r = g.getBoundingClientRect();
      return r.width > 0 && r.left >= 0 && r.top < window.innerHeight;
    });
    if (!group) return null;
    const r = group.getBoundingClientRect();
    const over = [...group.querySelectorAll("*")].filter((n) => n.getBoundingClientRect().right > window.innerWidth + 1).length;
    const buttons = [...group.querySelectorAll(".attn-group-answer")].map((b) => b.getBoundingClientRect().height);
    const texts = [...group.querySelectorAll(".attn-member")].map((m) => m.textContent);
    return { members: group.querySelectorAll(".attn-members li").length, right: r.right, over, buttons, texts };
  });
  check(fit && fit.members === 3 && fit.over === 0 && fit.right <= 391, `phone: the group of 3 fits the screen (${JSON.stringify(fit)})`);
  check(fit && fit.buttons.every((h) => h >= 44), `phone: group buttons are tap-sized (${fit && fit.buttons})`);
  check(fit && fit.texts.every((t) => /^"w\d": task \d$/.test(t)), `phone: members say what each starts, once (${fit && JSON.stringify(fit.texts)})`);
  await pp.screenshot({ path: path.join(out, "phone-hold-group.png") });
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
fs.rmSync(state, { recursive: true, force: true });

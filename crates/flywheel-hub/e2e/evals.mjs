// End-to-end check of the Flywheel panel's Evals view in a real browser.
// A scratch home gets a frozen two-task set (one the scripted agent
// passes, one it fails), a real `fw eval replay` of two variants at k=2
// with a scripted `claude`, and a hub on a temp state dir. The checks:
//   - the drawer's Evals section lists the run with its progress and spend;
//   - "open" shows the board: four paired comparisons, each with a verdict;
//   - the task x variant grid has one mark per trial, pass and fail;
//   - a mark opens that trial: its oracle verdict, trace checks, and diff;
//   - on a phone the board fits the screen.
// Opt-in, like layout.mjs:
//
//   node crates/flywheel-hub/e2e/evals.mjs [OUT_DIR]
//
// Env: FW (default target/debug/fw), PLAYWRIGHT, CHROMIUM, PORT (as
// layout.mjs). Never touches the default state dir, port 7420, the user's
// flywheel home, or the machine's test-suite lock.
import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT || "playwright");

const root = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../../..");
const fw = process.env.FW || path.join(root, "target/debug/fw");
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "fw-hub-evals-e2e-"));
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

const home = path.join(tmp, "home");
const fly = path.join(tmp, "fly");
const bin = path.join(home, "bin");
const repo = path.join(tmp, "repo");
for (const d of [home, fly, bin, repo]) fs.mkdirSync(d, { recursive: true });
const env = {
  ...process.env,
  HOME: home,
  FLYWHEEL_HOME: fly,
  CLAUDE_CONFIG_DIR: path.join(home, "claude-config"),
  FLYWHEEL_EVAL_LOCK: path.join(tmp, "eval.lock"),
  GIT_CONFIG_NOSYSTEM: "1",
  SHELL: "/bin/sh",
};
for (const k of Object.keys(env)) {
  if (k.startsWith("FLYWHEEL_") && !["FLYWHEEL_HOME", "FLYWHEEL_EVAL_LOCK"].includes(k)) delete env[k];
}
delete env.CARGO_TARGET_DIR;

function run(cmd, args, cwd) {
  const r = spawnSync(cmd, args, { cwd, env, encoding: "utf8" });
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")}: ${r.status}\n${r.stdout}\n${r.stderr}`);
  return r.stdout;
}
const git = (...args) => run("git", args, repo).trim();
const commit = (msg, files) => {
  for (const [f, body] of Object.entries(files)) fs.writeFileSync(path.join(repo, f), body);
  git("add", "-A");
  git("commit", "-qm", msg);
  return git("rev-parse", "HEAD");
};

// The scripted agent: makes a.txt say 2 and commits (task "two" passes;
// task "three", which needs b.txt, fails); answers the canary as a Claude
// Code that honors --setting-sources.
fs.writeFileSync(path.join(bin, "claude"), `#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
if [ "$2" = "canary" ]; then
  case "$*" in
    *setting-sources=user*) touch "$CLAUDE_CONFIG_DIR/../hook-fired"
      echo '{"type":"system","subtype":"init","skills":["fw-eval-canary"],"mcp_servers":[{"name":"fw-eval-canary"}]}' ;;
    *) echo '{"type":"system","subtype":"init","skills":[],"mcp_servers":[]}' ;;
  esac
  exit 0
fi
echo 2 > a.txt
git add -A >/dev/null 2>&1 && git commit -qm "feat: two" >/dev/null 2>&1
E='<!-- LOOP_STATUS -->\\n{\\"intent\\":\\"verify\\",\\"reason\\":\\"done\\"}\\n<!-- /LOOP_STATUS -->'
printf '%s\\n' '{"type":"system","subtype":"init","session_id":"s","model":"claude-fake-1"}'
printf '%s\\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test -p x"}}],"usage":{"input_tokens":1,"output_tokens":1}}}'
printf '%s\\n' '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}'
printf '%s\\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":4,"total_cost_usd":0.4,"usage":{"input_tokens":7,"output_tokens":2}}'
`, { mode: 0o755 });

git("init", "-q", "-b", "main");
git("config", "user.email", "e2e@example.invalid");
git("config", "user.name", "eval e2e");
git("config", "commit.gpgsign", "false");
const base = commit("base", { "CLAUDE.md": "# rules\nRULE: skip check.sh\n", "a.txt": "1\n" });
const ref2 = commit("feat: two", { "a.txt": "2\n" });
const ref3 = commit("feat: three", { "b.txt": "x\n" });
const project = run(fw, ["project", "path"], repo).trim();
const projectId = path.basename(project);

// A draft set of the two tasks, written as the miner writes one.
const draft = path.join(project, "evals/sets/core/draft");
const task = (id, goal, b, r, oracle) => {
  fs.mkdirSync(path.join(draft, "tasks"), { recursive: true });
  fs.writeFileSync(path.join(draft, "tasks", `${id}.json`), JSON.stringify({ id, goal, graders: [{ type: "command", run: "true" }] }));
  fs.mkdirSync(path.join(draft, "oracles", id, "home"), { recursive: true });
  fs.writeFileSync(path.join(draft, "oracles", id, "home", "o.sh"), oracle);
  fs.writeFileSync(path.join(draft, "oracles", id, "registration.json"),
    JSON.stringify({ graders: [{ type: "command", name: "oracle", run: "sh @EVAL_PROJECT_HOME@/o.sh" }] }));
  return { id, source: "landed", base: b, reference: r, prior_cost_usd: 0.5 };
};
fs.mkdirSync(draft, { recursive: true });
fs.writeFileSync(path.join(draft, "set.json"), JSON.stringify({
  name: "core", project_id: projectId, repo, created_at: new Date().toISOString(),
  tasks: [
    task("three", "make b.txt say x", ref2, ref3, 'test "$(cat b.txt 2>/dev/null)" = x\n'),
    task("two", "make a.txt say 2", base, ref2, 'test "$(cat a.txt)" = 2\n'),
  ],
}));
const variants = path.join(project, "evals/variants");
fs.mkdirSync(variants, { recursive: true });
const pathEnv = `[env]\nPATH = "${bin}:${process.env.PATH}"\n`;
fs.writeFileSync(path.join(variants, "rule-on.toml"), `model = "fake"\n${pathEnv}`);
fs.writeFileSync(path.join(variants, "rule-off.toml"),
  `model = "fake"\n${pathEnv}[[repo]]\npath = "CLAUDE.md"\nop = "strip"\npattern = '(?m)^RULE: skip check\\.sh\\n'\n`);

run(fw, ["eval", "set", "validate", "core@draft", "--no-noise", "--timeout", "60s"], repo);
run(fw, ["eval", "set", "freeze", "core"], repo);
const replayed = spawnSync(fw, ["eval", "replay", "core", "--variant", "rule-on", "--variant", "rule-off", "-k", "2",
  "--parallel", "2", "--budget", "10", "--worker-timeout", "120s"], { cwd: repo, env, encoding: "utf8" });
check(replayed.status === 0, `fw eval replay ran every cell (exit ${replayed.status}) ${replayed.stderr.slice(-400)}`);
const runId = (replayed.stdout.match(/run (eval-\S+): /) || [])[1];
check(!!runId, `the replay named its run (${runId})`);

const state = path.join(tmp, "state");
const port = process.env.PORT ? Number(process.env.PORT) : await freePort();
const baseUrl = `http://127.0.0.1:${port}`;
const hub = spawn(fw, ["hub", "serve", "--state-dir", state, "--port", String(port)], {
  env: { ...env, FW_BIN: fw },
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
    const response = await fetch(baseUrl + url, {
      method,
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await response.text();
    if (!response.ok) throw new Error(`${method} ${url}: ${response.status} ${text}`);
    return text ? JSON.parse(text) : null;
  };
  const ws = await api("POST", "/api/workspaces", { name: "evals", folders: [repo] });
  const report = await api("GET", `/api/flywheel/eval?folder=${encodeURIComponent(repo)}&run=${runId}`);
  check(report.schema === 1 && report.trials_total === 8, `the API serves the report (${report.trials_graded}/${report.trials_total})`);

  browser = await chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
  const desk = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await desk.newPage();
  await page.goto(`${baseUrl}/login?token=${token}`);
  await page.waitForURL(`${baseUrl}/`);
  // The workspace's folders (with their Flywheel chip) show in its list.
  await page.locator(`#rail-tiles .rail-tile[data-id="${ws.id}"]`).click();
  await page.waitForSelector(".fw-chip", { timeout: 30000 });
  await page.locator(".fw-chip").first().click();
  await page.waitForSelector("[data-ev-open]", { timeout: 30000 }).catch(() => {});
  const row = await page.evaluate(() => {
    const r = document.querySelector("li.ev-run");
    return r ? { text: r.textContent, segs: r.querySelectorAll(".fw-seg").length } : null;
  });
  check(row && /core@v1/.test(row.text) && /rule-on vs rule-off/.test(row.text) && /8\/8 trials/.test(row.text),
    `the Evals section lists the run (${row && row.text})`);
  check(row && row.segs >= 2, `its bar shows passes and fails (${row && row.segs} segments)`);
  await page.screenshot({ path: path.join(out, "desktop-evals-section.png") });

  await page.locator("[data-ev-open]").first().click();
  await page.waitForSelector("#dlg-eval[open] .ev-cmp", { timeout: 15000 }).catch(() => {});
  const boardShape = await page.evaluate(() => ({
    cmps: [...document.querySelectorAll("#dlg-eval .ev-cmp")].map((c) => [c.dataset.evMetric, c.dataset.evVerdict]),
    passes: document.querySelectorAll("#dlg-eval .ev-mark.pass").length,
    fails: document.querySelectorAll("#dlg-eval .ev-mark.fail").length,
    rows: document.querySelectorAll("#dlg-eval .ev-grid tbody tr").length,
    overall: document.querySelectorAll("#dlg-eval .ev-table tbody tr").length,
  }));
  check(boardShape.cmps.length === 4 && boardShape.cmps.every(([, v]) => ["better", "worse", "tie", "no_information"].includes(v)),
    `the board compares on four metrics, each with a verdict (${JSON.stringify(boardShape.cmps)})`);
  check(boardShape.rows === 2 && boardShape.passes === 4 && boardShape.fails === 4,
    `the grid has one mark per trial (${boardShape.passes} pass, ${boardShape.fails} fail, ${boardShape.rows} tasks)`);
  check(boardShape.overall === 2, `the overall table has a row per variant (${boardShape.overall})`);
  await page.screenshot({ path: path.join(out, "desktop-eval-board.png") });

  // A passing trial: its facts, trace checks, and diff.
  await page.locator("#dlg-eval .ev-mark.pass").first().click();
  await page.waitForSelector("#dlg-eval .ev-trial .ev-facts", { timeout: 15000 }).catch(() => {});
  const trial = await page.evaluate(() => {
    const t = document.querySelector("#dlg-eval .ev-trial");
    if (!t) return null;
    const facts = Object.fromEntries([...t.querySelectorAll(".ev-facts > div")].map((d) => [d.querySelector("dt").textContent, d.querySelector("dd").textContent]));
    return { facts, diff: !!t.querySelector(".ev-pre.diff"), open: t.dataset.evTrialOpen };
  });
  check(trial && trial.facts.oracle === "pass" && Number(trial.facts["tool calls"]) >= 1,
    `a mark opens its trial: oracle and trace checks (${trial && JSON.stringify(trial.facts)})`);
  check(trial && trial.diff, "the trial shows its final diff");
  await page.screenshot({ path: path.join(out, "desktop-eval-trial.png") });

  const phone = await browser.newContext({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });
  const pp = await phone.newPage();
  await pp.goto(`${baseUrl}/login?token=${token}`);
  await pp.waitForURL(`${baseUrl}/`);
  // A phone reaches the folder's chip through the menu.
  await pp.locator(".menu-toggle").first().click();
  await pp.locator(`#rail-tiles .rail-tile[data-id="${ws.id}"]`).click();
  await pp.locator(".fw-chip").first().click();
  await pp.waitForSelector("[data-ev-open]", { timeout: 30000 }).catch(() => {});
  if (await pp.locator("[data-ev-open]").count()) await pp.locator("[data-ev-open]").first().click();
  await pp.waitForSelector("#dlg-eval[open] .ev-cmp", { timeout: 15000 }).catch(() => {});
  const fit = await pp.evaluate(() => {
    const sheet = document.querySelector("#dlg-eval[open]");
    if (!sheet) return null;
    const over = [...sheet.querySelectorAll(".ev-cmp, .ev-block h3, .fo-head")].filter((n) => n.getBoundingClientRect().right > window.innerWidth + 1).length;
    return { right: sheet.getBoundingClientRect().right, over };
  });
  check(fit && fit.right <= 391 && fit.over === 0, `phone: the board fits the screen (${JSON.stringify(fit)})`);
  await pp.screenshot({ path: path.join(out, "phone-eval-board.png") });
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
fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 3 });

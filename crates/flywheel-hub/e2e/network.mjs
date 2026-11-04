// End-to-end check of the hub on a phone over a weak cellular link
// (opt-in; not part of scripts/check.sh, which has no node step).
//
//   node crates/flywheel-hub/e2e/network.mjs [--baseline] [--full]
//
// A 390x844 touch browser reaches the hub through a shaping proxy -- 400
// kbps down, 150 kbps up, 300 ms RTT, one link shared by every connection,
// with backpressure, and drops on demand -- and attaches to a chatty fake
// agent and an idle one. Measured: time to first screen, bytes per minute
// idle and busy, input echo latency, and a 10 s drop (keystrokes typed
// during it must arrive, in order, once the link is back). The proxy, not
// DevTools throttling: Chromium's Network.emulateNetworkConditions applies
// latency but neither throughput nor offline to WebSockets (checked in
// Chromium 127 and 156), and the hub must feel the slow reader too.
// `--baseline` prints the numbers without asserting the thresholds below;
// `--full` turns lite mode off (raw passthrough, the pre-lite behaviour).
//
// Env: FW (the fw binary; default target/debug/fw), PLAYWRIGHT (path to
// the playwright package; default: resolved from node's paths), CHROMIUM
// (a browser executable, when playwright's own is not installed), PORT (the
// hub's port; default: a free one), SHOTS (a directory for screenshots of
// the drop and the changed session). Runs a hub on a temp state dir with a
// fake `claude` first on PATH; never touches the default state dir or port
// 7420. Exit 0 = every check passed.
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT || "playwright");

const baseline = process.argv.includes("--baseline");
const root = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../../..");
const fw = process.env.FW || path.join(root, "target/debug/fw");
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "fw-hub-net-"));

// Thresholds (set from the baseline; see the commit that added them).
const LIMITS = {
  // Baseline (raw stream, uncompressed assets without validators):
  // cold load 37.4 s / 1.88 MB, reload 46.8 s / 2.38 MB, attach 1.6 s,
  // busy socket 62 KB/min, idle page 911 KB/min (assets fetched again by
  // the service worker), echo 306 ms, reconnect 6.4 s, keys typed in a
  // drop lost.
  cold_load_ms: 20000,
  reload_first_screen_ms: 6000,
  first_screen_ms: 1500,
  busy_bytes_per_min: 30 * 1024,
  idle_bytes_per_min: 16 * 1024,
  echo_ms: 900,
  reconnect_ms: 3500,
};

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

// The phone's link: one shaped pipe per direction shared by every
// connection (a chunk waits for the ones before it), RTT/2 each way, and
// backpressure (a connection with more than BUFFER bytes in flight stops
// being read, so the hub's socket fills as it would on a real link).
// `cut()` drops every connection and refuses new ones until `restore()`.
const LINK = { downBps: 400000, upBps: 150000, rttMs: 300 };
const BUFFER = 64 * 1024;
function shapingProxy(target) {
  const busyUntil = { down: 0, up: 0 };
  const counted = { down: 0, up: 0 };
  // Down bytes by what was asked for last on their connection.
  const byPath = new Map();
  const live = new Set();
  let refusing = false;
  function pipe(from, to, dir, bps, conn) {
    let inFlight = 0;
    from.on("data", (chunk) => {
      counted[dir] += chunk.length;
      if (dir === "up" && /^(GET|POST|PUT|DELETE) /.test(chunk.toString("latin1", 0, 8))) {
        const line = chunk.toString("latin1").split("\r\n")[0];
        conn.path = line.split(" ")[1].replace(/[0-9a-f]{32}/g, "ID").replace(/\?.*/, "");
        if (process.env.NETLOG) console.error(`${new Date().toISOString().slice(11, 23)} ${line}`);
      }
      if (dir === "down") byPath.set(conn.path, (byPath.get(conn.path) || 0) + chunk.length);
      for (let at = 0; at < chunk.length; at += 4096) {
        const piece = chunk.subarray(at, at + 4096);
        const now = Date.now();
        const start = Math.max(now, busyUntil[dir]);
        busyUntil[dir] = start + (piece.length * 8 * 1000) / bps;
        const due = busyUntil[dir] + LINK.rttMs / 2;
        inFlight += piece.length;
        if (inFlight > BUFFER) from.pause();
        setTimeout(() => {
          inFlight -= piece.length;
          if (!to.destroyed) to.write(piece);
          if (inFlight <= BUFFER / 2 && from.isPaused()) from.resume();
        }, due - now);
      }
    });
  }
  const server = net.createServer((client) => {
    if (refusing) {
      client.destroy();
      return;
    }
    const upstream = net.connect(target, "127.0.0.1");
    const pair = { client, upstream };
    live.add(pair);
    // One side closed: the other closes once what is in flight to it
    // was delivered.
    const closing = (other, dir) => () => {
      setTimeout(() => {
        live.delete(pair);
        other.destroy();
      }, Math.max(0, busyUntil[dir] - Date.now()) + LINK.rttMs / 2 + 20);
    };
    client.on("error", () => {}).on("close", closing(upstream, "up"));
    upstream.on("error", () => {}).on("close", closing(client, "down"));
    const conn = { path: "?" };
    pipe(upstream, client, "down", LINK.downBps, conn);
    pipe(client, upstream, "up", LINK.upBps, conn);
  });
  return {
    listen: (port) => new Promise((resolve) => server.listen(port, "127.0.0.1", resolve)),
    bytes: () => counted.down + counted.up,
    down: () => counted.down,
    // Down bytes by path since `since` (a previous result).
    paths(since) {
      const now = Object.fromEntries(byPath);
      if (!since) return now;
      const diff = {};
      for (const [k, v] of Object.entries(now)) if (v - (since[k] || 0) > 0) diff[k] = v - (since[k] || 0);
      return diff;
    },
    cut() {
      refusing = true;
      for (const { client, upstream } of live) {
        client.destroy();
        upstream.destroy();
      }
      live.clear();
    },
    restore() {
      refusing = false;
    },
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

// A fake Claude Code: a banner and a prompt; typed lines are answered.
// "work" starts what a busy agent draws: a spinner line redrawn ~12 times
// a second and a coloured log line every 300 ms (scrolling).
const bin = path.join(tmp, "bin");
fs.mkdirSync(bin);
const chatty = path.join(tmp, "chatty.sh");
fs.writeFileSync(chatty, `#!/bin/sh
i=0
while :; do
  i=$((i+1))
  for s in '|' '/' '-' '\\\\'; do
    printf '\\r\\033[2K\\033[38;5;208m%s\\033[0m Working... \\033[2m(%ss, esc to interrupt)\\033[0m' "$s" "$i"
    sleep 0.08
  done
  printf '\\r\\033[2K\\033[32m*\\033[0m step %s: \\033[1mreading\\033[0m src/module_%s.rs (%s lines)\\n' "$i" "$((i % 17))" "$((i * 37 % 900))"
done
`, { mode: 0o755 });
fs.writeFileSync(path.join(bin, "claude"), `#!/bin/sh
echo 'fake claude'
printf '> '
while IFS= read -r line; do
  case "$line" in
    work*) sh '${chatty}' & ;;
    quit*) exit 0 ;;
    *) printf 'you said: %s\\n' "$line" ;;
  esac
  printf '> '
done
`, { mode: 0o755 });
const repo = path.join(tmp, "repo");
fs.mkdirSync(repo, { recursive: true });
const state = path.join(tmp, "state");

const port = process.env.PORT ? Number(process.env.PORT) : await freePort();
const base = `http://127.0.0.1:${port}`;
const hub = spawn(fw, ["hub", "serve", "--state-dir", state, "--port", String(port), "--no-flywheel"], {
  env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, SHELL: "/bin/sh", FW_BIN: fw },
  stdio: ["ignore", "ignore", "inherit"],
});

const metrics = {};

let browser;
let link;
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

  const ws = await api("POST", "/api/workspaces", { name: "net", folders: [repo] });
  const busy = await api("POST", "/api/sessions", { workspace_id: ws.id, kind: "claude", cols: 48, rows: 30 });
  const idle = await api("POST", "/api/sessions", { workspace_id: ws.id, kind: "claude", cols: 48, rows: 30 });
  await sleep(500);
  await api("POST", `/api/sessions/${busy.id}/input`, { data: "work", enter: true });
  // Let the busy agent build up scrollback, as a long turn does.
  await sleep(20000);

  link = shapingProxy(port);
  const proxyPort = await freePort();
  await link.listen(proxyPort);
  const phoneBase = `http://127.0.0.1:${proxyPort}`;
  browser = await chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
  const phoneCtx = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, deviceScaleFactor: 2 });
  const page = await phoneCtx.newPage();
  const total = () => link.bytes();
  let t0 = Date.now();

  t0 = Date.now();
  await page.goto(`${phoneBase}/login?token=${token}`, { timeout: 180000 });
  await page.waitForURL(`${phoneBase}/`, { timeout: 180000 });
  await page.waitForFunction(() => typeof state === "object" && state.sessions && state.sessions.length > 0, null, { timeout: 180000 });
  metrics.cold_load_ms = Date.now() - t0;
  metrics.cold_load_bytes = total();
  await page.waitForTimeout(3000);
  if (process.argv.includes("--full")) {
    await page.evaluate(() => settings.set("terminal.lite", "off", { device: true }));
  }

  const screenText = () => page.evaluate(() => {
    const pane = state.solo;
    if (!pane || !pane.term) return "";
    const buf = pane.term.buffer.active;
    const rows = [];
    for (let y = buf.viewportY; y < buf.viewportY + pane.term.rows; y++) rows.push(buf.getLine(y)?.translateToString(true) || "");
    return rows.join("\n");
  });
  const linkKind = () => page.evaluate(() => state.solo && state.solo.link.kind);

  const busyShown = () => page.waitForFunction(() => {
    const pane = state.solo;
    if (!pane || !pane.term || pane.link.kind !== "attached" || !pane.drawn) return false;
    const buf = pane.term.buffer.active;
    for (let y = buf.viewportY; y < buf.viewportY + pane.term.rows; y++) {
      if (/Working\.\.\./.test(buf.getLine(y)?.translateToString(true) || "")) return true;
    }
    return false;
  }, null, { timeout: 180000, polling: 50 });

  // 1. Time to first screen: attach the busy agent from the start
  // screen, then again on a reload (assets already fetched once).
  t0 = Date.now();
  let b0 = total();
  await page.evaluate((id) => showSolo(id), busy.id);
  await busyShown();
  metrics.first_screen_ms = Date.now() - t0;
  metrics.first_screen_bytes = total() - b0;
  t0 = Date.now();
  b0 = total();
  await page.evaluate((id) => { location.hash = `session=${id}`; }, busy.id);
  await page.reload({ timeout: 180000 });
  await busyShown();
  metrics.reload_first_screen_ms = Date.now() - t0;
  metrics.reload_bytes = total() - b0;

  // 2. Bytes a minute attached to the busy agent (30 s, scaled).
  await page.waitForTimeout(3000);
  b0 = total();
  let p0 = link.paths();
  await page.waitForTimeout(30000);
  metrics.busy_bytes_per_min = (total() - b0) * 2;
  metrics.busy_paths = link.paths(p0);

  // 3. ... and to the idle one.
  await page.evaluate((id) => showSolo(id), idle.id);
  await page.waitForFunction(() => state.solo && state.solo.link.kind === "attached" && state.solo.drawn, null, { timeout: 60000 });
  await page.waitForTimeout(5000);
  b0 = total();
  p0 = link.paths();
  await page.waitForTimeout(30000);
  metrics.idle_bytes_per_min = (total() - b0) * 2;
  metrics.idle_paths = link.paths(p0);

  // 4. Input echo: a key typed until the terminal shows it (the tty's
  // echo, round trip through the hub).
  await page.evaluate(() => state.solo.term.focus());
  const echoes = [];
  for (const ch of "qwert") {
    const before = await screenText();
    const count = (s) => s.split(ch).length;
    const t = Date.now();
    await page.keyboard.type(ch);
    await page.waitForFunction(({ ch, n }) => {
      const pane = state.solo;
      const buf = pane.term.buffer.active;
      let text = "";
      for (let y = buf.viewportY; y < buf.viewportY + pane.term.rows; y++) text += buf.getLine(y)?.translateToString(true) || "";
      return text.split(ch).length > n;
    }, { ch, n: count(before.replace(/\n/g, "")) }, { timeout: 30000, polling: 20 });
    echoes.push(Date.now() - t);
  }
  echoes.sort((a, b) => a - b);
  metrics.echo_ms = echoes[Math.floor(echoes.length / 2)];
  await page.keyboard.press("Enter");
  await page.waitForTimeout(1500);

  // 5. A 10 s drop: the link cut (sockets die), two lines typed
  // meanwhile, then the link back. Time until reattached; the lines must
  // arrive, in order, once.
  metrics.lite = await page.evaluate(() => net.lite());
  link.cut();
  await page.waitForTimeout(1500);
  metrics.drop_link = await linkKind();
  await page.keyboard.type("zz9");
  await page.keyboard.press("Enter");
  await page.keyboard.type("yy8");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(500);
  metrics.drop_banner = await page.evaluate(() => {
    const b = state.solo.el.querySelector(".pane-banner");
    return b && !b.hidden ? b.textContent : "";
  });
  metrics.drop_badge = await page.evaluate(() => {
    const b = state.solo.el.querySelector(".pane-pending");
    return b && !b.hidden ? b.textContent : "";
  });
  if (process.env.SHOTS) await page.screenshot({ path: path.join(process.env.SHOTS, "phone-drop-queued.png") });
  await page.waitForTimeout(8000);
  b0 = total();
  p0 = link.paths();
  t0 = Date.now();
  link.restore();
  await page.waitForFunction(() => state.solo && state.solo.link.kind === "attached" && state.solo.drawn && state.solo.ready, null, { timeout: 60000, polling: 50 });
  metrics.reconnect_ms = Date.now() - t0;
  const typedBack = async (re) => page.waitForFunction((source) => {
    const pane = state.solo;
    const buf = pane.term.buffer.active;
    const lines = [];
    for (let y = 0; y < buf.length; y++) lines.push(buf.getLine(y)?.translateToString(true) || "");
    return new RegExp(source, "s").test(lines.join("\n"));
  }, re.source, { timeout: 15000, polling: 50 }).then(() => true, () => false);
  metrics.drop_input_kept = await typedBack(/you said: zz9\n.*you said: yy8/);
  metrics.drop_input_once = (await screenText()).split("you said: zz9").length === 2;
  metrics.reconnect_bytes = total() - b0;
  metrics.reconnect_paths = link.paths(p0);

  // 6. The session changes underneath during a drop (it ended and was
  // relaunched): what was typed meanwhile is not sent, and it says so.
  link.cut();
  await page.waitForTimeout(1000);
  await page.keyboard.type("lost1");
  await page.keyboard.press("Enter");
  await api("POST", `/api/sessions/${idle.id}/input`, { data: "quit", enter: true });
  // Relaunch once it has ended (409 while it runs).
  for (let i = 0; ; i++) {
    try {
      await api("POST", `/api/sessions/${idle.id}/relaunch`, {});
      break;
    } catch (error) {
      if (i > 100) throw error;
      await sleep(100);
    }
  }
  await sleep(1000);
  link.restore();
  await page.waitForFunction(() => state.solo && state.solo.link.kind === "attached" && state.solo.ready, null, { timeout: 60000, polling: 50 });
  await page.waitForTimeout(1500);
  metrics.changed_banner = await page.evaluate(() => {
    const b = state.solo.el.querySelector(".pane-banner");
    return b && !b.hidden ? b.textContent : "";
  });
  metrics.changed_input_dropped = !(await screenText()).includes("you said: lost1");
  if (process.env.SHOTS) await page.screenshot({ path: path.join(process.env.SHOTS, "phone-session-changed.png") });
  metrics.drop_screen = (await screenText()).split("\n").filter(Boolean).slice(-4).join(" | ");
  await phoneCtx.close();
} finally {
  if (browser) await browser.close();
  if (link) await link.close();
  hub.kill("SIGTERM");
  await new Promise((r) => hub.once("exit", r));
}

console.log(JSON.stringify(metrics, null, 2));
if (!baseline) {
  check(metrics.cold_load_ms <= LIMITS.cold_load_ms, `cold load in ${metrics.cold_load_ms} ms, ${metrics.cold_load_bytes} B (<= ${LIMITS.cold_load_ms})`);
  check(metrics.reload_first_screen_ms <= LIMITS.reload_first_screen_ms, `reload to a busy agent's screen in ${metrics.reload_first_screen_ms} ms, ${metrics.reload_bytes} B (<= ${LIMITS.reload_first_screen_ms})`);
  check(metrics.first_screen_ms <= LIMITS.first_screen_ms, `attach to a busy agent in ${metrics.first_screen_ms} ms (<= ${LIMITS.first_screen_ms})`);
  check(metrics.busy_bytes_per_min <= LIMITS.busy_bytes_per_min, `busy attached: ${metrics.busy_bytes_per_min} B/min (<= ${LIMITS.busy_bytes_per_min})`);
  check(metrics.idle_bytes_per_min <= LIMITS.idle_bytes_per_min, `idle attached: ${metrics.idle_bytes_per_min} B/min (<= ${LIMITS.idle_bytes_per_min})`);
  check(metrics.echo_ms <= LIMITS.echo_ms, `echo latency ${metrics.echo_ms} ms (<= ${LIMITS.echo_ms})`);
  check(metrics.reconnect_ms <= LIMITS.reconnect_ms, `reconnect after a 10 s drop in ${metrics.reconnect_ms} ms (<= ${LIMITS.reconnect_ms})`);
  if (!process.argv.includes("--full")) check(metrics.lite === true, "lite mode turned itself on (auto) over the slow link");
  check(/queued/.test(metrics.drop_badge) && /queued/.test(metrics.drop_banner), `the drop shows the keys queued (${metrics.drop_badge} / ${metrics.drop_banner})`);
  check(metrics.drop_input_kept === true && metrics.drop_input_once === true, "keystrokes typed during the drop arrive once the link is back, in order, once");
  check(metrics.changed_input_dropped === true && /not sent/.test(metrics.changed_banner), `a session changed underneath drops what was typed and says so (${metrics.changed_banner})`);
}
if (failures.length) {
  console.log(`${failures.length} check(s) failed`);
  process.exit(1);
}
fs.rmSync(tmp, { recursive: true, force: true });

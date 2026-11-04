// End-to-end check of the hub's layout in a real browser: tabs + split
// panes, the desktop sidebar, popovers, toasts over dialogs, long paths,
// and phone tap targets (opt-in; not part of scripts/check.sh, which has
// no node step).
//
//   node crates/flywheel-hub/e2e/layout.mjs [OUT_DIR]
//
// Env: FW (the fw binary; default target/debug/fw), PLAYWRIGHT (path to
// the playwright package; default: resolved from node's paths), CHROMIUM
// (a browser executable, when playwright's own is not installed), PORT (the
// hub's port; default: a free one). Runs a hub on a temp state dir and a
// port with a fake `claude` first on PATH (and git for one real repo);
// never touches the default state dir or port 7420. Screenshots go to
// OUT_DIR (default: a temp dir, printed). Exit 0 = every check passed.
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

// The drawers over the terminal (Commands, Review, Queue, Flywheel: this
// hub runs --no-flywheel, so its drawer opens on the attached session's
// folder and says it cannot read it, which is enough for its box): on
// desktop one width and one edge; on a phone a
// full-screen sheet no terminal shows around; and a toast never covers a
// drawer's controls (the drawer lifts them above the stack, its frame
// stays put, and drops the room once the toasts go).
// Every character real Claude Code sessions print (tests/fixtures/
// claude-glyphs.txt), and every non-ASCII one on the live rows, is drawn
// by the vendored terminal fonts (the stack's "FW ..." faces: JetBrains
// Mono, then the JuliaMono "FW Symbols"), never a device font or a tofu
// box. Asked of the fonts alone, on a canvas (Chromium's platform-font
// report names the stack's first font even for a box, and a device font
// can fill any gap): a character the vendored faces lack draws the same
// with them in front of a reference font as the reference alone (serif,
// and again monospace), and one no font has draws the .notdef box of an
// unassigned codepoint (U+0378). Characters known to be in no vendored
// face must be caught, or the check is blind.
const GLYPHS = fs.readFileSync(path.join(root, "crates/flywheel-hub/tests/fixtures/claude-glyphs.txt"), "utf8")
  .split("\n").filter((l) => l.startsWith("U+")).map((l) => parseInt(l.slice(2).split(" ")[0], 16))
  .filter((cp) => cp !== 0xa0);
// CJK, an emoji, Cyrillic: in no vendored face (the subsets are Latin and
// symbols), whatever the device has.
const ABSENT = [0x4e2d, 0x1f916, 0x0436];
async function glyphChecks(page, label) {
  const { stack, vendored, missing, caught, probes } = await page.evaluate(async ({ cps, absent }) => {
    const rows = document.querySelector(".xterm-rows");
    const stack = getComputedStyle(rows).fontFamily;
    const vendored = stack.split(",").map((f) => f.trim()).filter((f) => /^"?FW /.test(f)).join(", ");
    const live = [...new Set(rows.textContent)].filter((ch) => ch.codePointAt(0) > 0x7f && ch !== "\u00a0");
    const probes = [...new Set([...cps.map((cp) => String.fromCodePoint(cp)), ...live])];
    const all = probes.join("") + absent.map((cp) => String.fromCodePoint(cp)).join("");
    // Every face a probe needs (the lazy symbols one by unicode-range).
    await document.fonts.load(`24px ${vendored}`, all);
    await document.fonts.ready;
    const canvas = document.createElement("canvas");
    canvas.width = 48;
    canvas.height = 40;
    const g = canvas.getContext("2d", { willReadFrequently: true });
    const draw = (font, ch) => {
      g.font = `24px ${font}`;
      g.clearRect(0, 0, 48, 40);
      g.fillText(ch, 4, 30);
      return g.getImageData(0, 0, 48, 40).data.join(",");
    };
    const notdef = draw(vendored, "\u0378");
    const lacks = (ch) =>
      draw(vendored, ch) === notdef ||
      ["serif", "monospace"].every((ref) => draw(`${vendored}, ${ref}`, ch) === draw(ref, ch));
    const hex = (ch) => `U+${ch.codePointAt(0).toString(16).toUpperCase()}`;
    return {
      stack,
      vendored,
      missing: probes.filter(lacks).map(hex),
      caught: absent.map((cp) => String.fromCodePoint(cp)).filter(lacks).length,
      probes: probes.length,
    };
  }, { cps: GLYPHS, absent: ABSENT });
  check(stack.includes('"FW Symbols"'), `${label}: the terminal falls back to the vendored symbols (${stack})`);
  check(caught === ABSENT.length, `${label}: the glyph check sees a character no vendored font has (${caught}/${ABSENT.length})`);
  check(!missing.length, `${label}: all ${probes} characters (${GLYPHS.length} Claude Code + live rows) drawn by ${vendored} (missing ${JSON.stringify(missing)})`);
}

// Text shown exactly as written: JetBrains Mono's contextual forms and
// ligatures are off where terminals, transcripts, suggested commands,
// and the handoff text are drawn: "```", "->", "!=" stay their
// characters. Each place's computed features say so, and that text drawn
// in the terminal's own style matches it drawn with the features off,
// not on (on, JetBrains Mono joins "->" and "!=").
async function exactTextChecks(page, label, selectors) {
  const styles = await page.evaluate((sels) => sels.map((sel) => {
    const node = document.querySelector(sel);
    if (!node) return [sel, null];
    const cs = getComputedStyle(node);
    return [sel, `${cs.fontVariantLigatures} ${cs.fontFeatureSettings}`];
  }), selectors);
  const loose = styles.filter(([, style]) => !style || !/^none\b/.test(style) || !/"calt" 0/.test(style) || !/"liga" 0/.test(style));
  check(!loose.length, `${label}: no ligatures or contextual forms in ${selectors.join(" ")} (${JSON.stringify(loose)})`);
  if (!selectors.includes(".xterm-rows")) return;
  // Drawn over everything, in the terminal rows' font and (null) their
  // computed features, or the features given.
  const probe = async (features) => {
    await page.evaluate((f) => {
      const cs = getComputedStyle(document.querySelector(".xterm-rows"));
      const span = document.createElement("span");
      span.id = "e2e-ticks";
      span.textContent = "x```-> != y";
      span.style.cssText = `position:fixed;left:0;top:0;z-index:2147483647;display:inline-block;line-height:1.2;padding:4px;background:#000;color:#fff;font-size:28px;font-family:${cs.fontFamily}`;
      span.style.fontVariantLigatures = f ? "normal" : cs.fontVariantLigatures;
      span.style.fontFeatureSettings = f || cs.fontFeatureSettings;
      document.body.append(span);
    }, features);
    const shot = await page.locator("#e2e-ticks").screenshot();
    await page.evaluate(() => document.getElementById("e2e-ticks").remove());
    return shot.toString("base64");
  };
  const shown = await probe(null);
  const off = await probe('"calt" 0, "liga" 0');
  const on = await probe('"calt" 1, "liga" 1');
  check(on !== off, `${label}: JetBrains Mono's ligatures change "->" (the check can see them)`);
  check(shown === off, `${label}: the terminal draws "\`\`\`-> !=" character by character`);
}

async function drawerChecks(page, label, phone) {
  const boxes = {};
  for (const which of ["commands", "review", "queue", "flywheel"]) {
    await page.evaluate((w) => {
      if (w === "commands") commands.toggle(true);
      if (w === "review") review.open(state.attached);
      if (w === "queue") promptQueue.open(state.attached);
      if (w === "flywheel") {
        const s = sessionById(state.attached);
        flywheel.open(s.cwd || "/", s.workspace_id);
      }
    }, which);
    await page.waitForSelector(`#${which}:not([hidden])`, { timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(600);
    const before = await page.evaluate((w) => {
      const r = document.getElementById(w).getBoundingClientRect();
      return [r.left, r.top, r.right, r.bottom].map(Math.round);
    }, which);
    await page.evaluate(() => {
      for (let i = 0; i < 2; i++) commands.toast(`drawer toast ${i}, long enough to wrap on a phone`, { timeout: 0 });
    });
    await page.waitForTimeout(400);
    const r = await page.evaluate(({ w, phone }) => {
      const d = document.getElementById(w);
      if (d.hidden) return { open: false, bad: ["did not open"] };
      const box = d.getBoundingClientRect();
      const toasts = [...document.querySelectorAll(".toast")].map((t) => t.getBoundingClientRect()).filter((t) => t.width);
      const bad = [];
      if (!toasts.length) bad.push("no toast shown");
      for (const t of toasts) {
        const hit = document.elementFromPoint(t.left + t.width / 2, t.top + t.height / 2);
        if (!hit || !hit.closest(".toast")) bad.push(`toast under ${hit ? hit.id || hit.className : "nothing"}`);
      }
      // Every part of the drawer (head, body, footer) ends above any toast
      // over it: nothing inside can be under one.
      for (const part of d.children) {
        const p = part.getBoundingClientRect();
        if (!p.width || !p.height) continue;
        for (const t of toasts) {
          if (p.left < t.right && p.right > t.left && p.top < t.bottom && p.bottom > t.top + 1) bad.push(`${part.id || part.className} under a toast`);
        }
      }
      if (phone) {
        if (Math.round(box.left) !== 0 || Math.round(box.top) !== 0 || Math.round(box.right) !== innerWidth || Math.round(box.bottom) !== innerHeight) {
          bad.push(`not full screen ${JSON.stringify([box.left, box.top, box.right, box.bottom].map(Math.round))}`);
        }
        let peeks = 0;
        for (let y = 0; y < innerHeight; y += 3) for (let x = 0; x < innerWidth; x += 3) {
          const e = document.elementFromPoint(x, y);
          if (e && e.closest("#terminal, #keybar, #term-header")) peeks += 1;
        }
        if (peeks) bad.push(`terminal or key bar shows at ${peeks} points`);
      }
      return { open: true, box: [box.left, box.top, box.right, box.bottom].map(Math.round), bad };
    }, { w: which, phone });
    await page.screenshot({ path: path.join(out, `${label}-drawer-${which}-toast.png`) });
    if (which === "commands") {
      // The pickers show their choice whole, and a phone's hint names no
      // keyboard shortcut.
      const fit = await page.evaluate(() => [...document.querySelectorAll("#commands .cmd-field select")].map((select) => {
        const cs = getComputedStyle(select);
        const g = document.createElement("canvas").getContext("2d");
        g.font = `${cs.fontWeight} ${cs.fontSize} ${cs.fontFamily}`;
        const text = select.selectedOptions[0] ? select.selectedOptions[0].textContent : "";
        const room = select.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight) - 20;
        return { id: select.id, text, fits: g.measureText(text).width <= room };
      }));
      const target = fit.find((f) => f.id === "cmd-target");
      check(target && target.fits, `${label}: the commands drawer's send-to choice is shown whole (${JSON.stringify(target)})`);
      if (phone) {
        const hint = await page.evaluate(() => document.querySelector("#commands").textContent);
        check(!/\bAlt\b|Ctrl|⌘/.test(hint), `${label}: the commands drawer names no keyboard shortcut on a phone`);
      }
    }
    const after = await page.evaluate(async (w) => {
      for (const n of [...document.querySelectorAll("#toasts .toast")]) n.remove();
      await new Promise((res) => setTimeout(res, 300));
      const d = document.getElementById(w);
      return { pad: getComputedStyle(d).paddingBottom, docked: d.classList.contains("toast-docked") };
    }, which);
    const kept = JSON.stringify(before) === JSON.stringify(r.box);
    check(r.open && r.bad.length === 0 && kept && !after.docked, `${label}: a toast over the ${which} drawer is on top and covers none of it; the drawer keeps its box (${JSON.stringify({ ...r, before, after })})`);
    boxes[which] = r.box;
    await page.evaluate(() => closeOverlays(""));
    await page.waitForTimeout(300);
  }
  if (!phone) {
    const all = Object.values(boxes).map((b) => JSON.stringify(b));
    check(all.every((b) => b === all[0]), `desktop: every drawer has one width and one edge (${JSON.stringify(boxes)})`);
  }
}

// A fake Claude Code: prints a banner and a prompt, then echoes input.
// Asked to "polish", it also writes a transcript with usage (the header's
// meter) and reports itself blocked (the why? button). A fan-out's "do it"
// finishes its turn at once (the hub then grades it).
const bin = path.join(tmp, "bin");
fs.mkdirSync(bin);
const claudeConfig = path.join(tmp, "claude-config");
fs.mkdirSync(path.join(claudeConfig, "projects", "e2e"), { recursive: true });
fs.writeFileSync(path.join(bin, "claude"), `#!/bin/sh
case "$*" in
*polish*)
  id=""
  for a in "$@"; do [ "$prev" = "--session-id" ] && id="$a"; prev="$a"; done
  printf '%s\\n' '{"type":"assistant","uuid":"u1","timestamp":"2026-11-03T10:00:00.000Z","message":{"id":"m1","model":"claude-sonnet-5-5","role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1200,"output_tokens":300,"cache_read_input_tokens":40000}}}' > "$CLAUDE_CONFIG_DIR/projects/e2e/$id.jsonl"
  "$FW_BIN" hub report --source claude --event Notification --detail permission_prompt </dev/null >/dev/null 2>&1
  ;;
*"do it"*)
  "$FW_BIN" hub report --source claude --event Stop </dev/null >/dev/null 2>&1
  ;;
esac
echo 'fake claude'
printf '> '
# Typed "e2e-go" / "e2e-stop": a turn starts / finishes (seen-on-view).
while IFS= read -r line; do
  case "$line" in
  *e2e-go*) "$FW_BIN" hub report --source claude --event UserPromptSubmit </dev/null >/dev/null 2>&1 ;;
  *e2e-stop*) "$FW_BIN" hub report --source claude --event Stop </dev/null >/dev/null 2>&1 ;;
  esac
  printf '%s\n' "$line"
done
`, { mode: 0o755 });
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
  await glyphChecks(page, "desktop");
  await exactTextChecks(page, "desktop", [".xterm-rows", "#cmd-list", "#tx-body", "#ho-text"]);
  // Session and workspace names in a face that tells 1 from l and 0 from
  // O ("c1" and "cl" read alike in the condensed display face).
  const nameFaces = await page.evaluate(() => [".ws-name", ".session-name", ".crumb-session", ".crumb-ws", ".pane-name", ".tab-list [role=tab]", "#rv-title", "#q-title", "#tx-title"]
    .map((sel) => [sel, document.querySelector(sel) && getComputedStyle(document.querySelector(sel)).fontFamily]));
  const displayed = nameFaces.filter(([, face]) => !face || !/^"FW (Mono|Term)"/.test(face));
  check(!displayed.length, `desktop: names in the UI mono (${JSON.stringify(displayed)})`);
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
  await page.locator(`.session[data-id="${other.id}"]`).click();
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
  // Default names count per program (claude 1, sh 1, sh 2): pick the last shell.
  const lastShell = (await api("GET", `/api/sessions/${sids[2]}`)).name;
  await page.locator("#empty-actions button", { hasText: lastShell }).click();
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

  // --- desktop 1440x900: sidebar, popovers, toasts, long paths --------------
  // A real git repo at a long path: a worktree agent and a fan-out.
  const repo3 = path.join(tmp, "code", "a-really-quite-long-repository-directory-name-to-see-truncation");
  fs.mkdirSync(repo3, { recursive: true });
  const git = (...args) => execFileSync("git", ["-C", repo3, ...args], {
    stdio: "ignore",
    env: { ...process.env, GIT_AUTHOR_NAME: "e2e", GIT_AUTHOR_EMAIL: "e2e@example.invalid", GIT_COMMITTER_NAME: "e2e", GIT_COMMITTER_EMAIL: "e2e@example.invalid" },
  });
  git("init", "-q", "-b", "main");
  fs.writeFileSync(path.join(repo3, "README"), "e2e\n");
  git("add", "README");
  git("commit", "-qm", "init");
  const ws3 = await api("POST", "/api/workspaces", { name: "polish", folders: [repo3] });
  const task = "another-deliberately-long-task-name-for-the-path-check";
  const agent = await api("POST", "/api/sessions", { workspace_id: ws3.id, kind: "claude", prompt: "polish", worktree: { task }, cols: 100, rows: 30 });
  // The fan-out in a workspace of its own (another real repo), so its
  // rollup is the candidates' alone.
  const repo4 = path.join(tmp, "code", "fanned");
  fs.mkdirSync(repo4, { recursive: true });
  for (const args of [["init", "-q", "-b", "main"], ["commit", "-q", "--allow-empty", "-m", "init"]]) {
    execFileSync("git", ["-C", repo4, ...args], {
      stdio: "ignore",
      env: { ...process.env, GIT_AUTHOR_NAME: "e2e", GIT_AUTHOR_EMAIL: "e2e@example.invalid", GIT_COMMITTER_NAME: "e2e", GIT_COMMITTER_EMAIL: "e2e@example.invalid" },
    });
  }
  const ws4 = await api("POST", "/api/workspaces", { name: "fanned", folders: [repo4] });
  const fo = await api("POST", "/api/fanouts", { workspace_id: ws4.id, prompt: "do it", candidates: [{ kind: "claude" }, { kind: "claude" }], graders: ["true"] });
  const pol = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const pp = await pol.newPage();
  await pp.goto(`${base}/login?token=${token}#session=${agent.id}`);
  await pp.waitForSelector("#terminal .pane .xterm", { timeout: 15000 });
  await pp.waitForFunction(() => !document.querySelector("#crumb-why").hidden && !document.querySelector("#meter-usage").hidden, null, { timeout: 15000 })
    .catch(() => {});
  // Needs you, the chip, and the hub's own rollup agree on a fan-out
  // candidate's finished turn: the hub grades it, so none counts it done;
  // the fan-out's unseen grading is the one done item, until its board is
  // opened (then it is gone from all three).
  const candidates = new Set(fo.candidates.map((c) => c.session_id));
  for (let i = 0; i < 100; i++) {
    const listed = await api("GET", "/api/fanouts");
    const f = listed.fanouts.find((x) => x.id === fo.id);
    if (f && f.graded_at && f.candidates.every((c) => c.verdict && c.verdict !== "ungraded")) break;
    await sleep(200);
  }
  await pp.waitForFunction((ids) => ids.every((id) => {
    const s = state.sessions.find((x) => x.id === id);
    return s && agentStatus(s)?.status === "done";
  }), [...candidates], { timeout: 15000 }).catch(() => {});
  await pp.waitForFunction((ws) => [...document.querySelectorAll("#attention .attn-row.fo .attn-ws")].some((n) => n.textContent === ws), "fanned", { timeout: 10000 }).catch(() => {});
  const agree = await pp.evaluate(({ ws, ids }) => {
    const rows = [...document.querySelectorAll("#attention .attn-row")];
    const here = rows.filter((r) => r.querySelector(".attn-ws")?.textContent === ws);
    const chip = document.querySelector(`section.workspace[data-id="${state.workspaces.find((w) => w.name === ws).id}"] .ws-status`);
    return {
      done: ids.map((id) => agentStatus(state.sessions.find((x) => x.id === id))?.status),
      listed: here.length,
      fanoutRow: here.some((r) => r.classList.contains("fo")),
      candidateRows: rows.filter((r) => ids.includes(r.dataset.id)).length,
      chip: chip ? chip.textContent : null,
      server: state.rollup[state.workspaces.find((w) => w.name === ws).id] || null,
    };
  }, { ws: "fanned", ids: [...candidates] });
  check(
    agree.done.every((s) => s === "done") && agree.fanoutRow && agree.candidateRows === 0 && agree.listed === 1 &&
      agree.chip === "done" && agree.server === "done",
    `desktop: Needs you, the chip and the hub's rollup count a fan-out's finished candidates as its one done (${JSON.stringify(agree)})`,
  );
  await pp.locator("#attention .attn-row.fo").first().click();
  await pp.waitForSelector("#dlg-fanout[open]", { timeout: 10000 }).catch(() => {});
  await pp.keyboard.press("Escape");
  await pp.waitForSelector("#dlg-fanout[open]", { state: "detached", timeout: 5000 }).catch(() => {});
  const fannedId = await pp.evaluate((ws) => state.workspaces.find((w) => w.name === ws).id, "fanned");
  await pp.waitForFunction((id) => state.rollup[id] !== "done", fannedId, { timeout: 10000 }).catch(() => {});
  const looked = await pp.evaluate((id) => ({
    rows: [...document.querySelectorAll("#attention .attn-row.fo")].length,
    chip: document.querySelector(`section.workspace[data-id="${id}"] .ws-status`)?.textContent || null,
    server: state.rollup[id] || null,
  }), fannedId);
  const listedAfter = (await api("GET", "/api/fanouts")).fanouts.find((x) => x.id === fo.id);
  check(
    looked.rows === 0 && looked.chip === null && looked.server !== "done" && listedAfter && listedAfter.needs_you === false,
    `desktop: opening the fan-out's board clears it from Needs you, the chip and the hub's rollup (${JSON.stringify({ ...looked, needs_you: listedAfter?.needs_you })})`,
  );
  const rectOf = (sel) => pp.evaluate((sel) => {
    const n = document.querySelector(sel);
    if (!n || n.hidden) return null;
    const r = n.getBoundingClientRect();
    return { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height };
  }, sel);
  // The sidebar's footer (actions + utilities) keeps to a strip, its
  // utilities whole, named, and not covered.
  const foot = await pp.evaluate(() => {
    const top = document.querySelector(".side-actions").getBoundingClientRect().top;
    const bad = [];
    for (const n of document.querySelectorAll(".side-foot .chip")) {
      const r = n.getBoundingClientRect();
      const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      if (!hit || !n.contains(hit)) bad.push(`${n.id || n.dataset.action} covered`);
      if (r.bottom > innerHeight || r.right > document.querySelector("#sidebar").getBoundingClientRect().right) bad.push(`${n.id || n.dataset.action} cut off`);
      if (!n.textContent.trim()) bad.push(`${n.id || n.dataset.action} has no name`);
    }
    return { height: Math.round(innerHeight - top), bad };
  });
  check(foot.height <= 180 && foot.bad.length === 0, `desktop: sidebar footer is ${foot.height}px (at most 180), every utility whole and named (${foot.bad.join(", ")})`);
  // The phone's More row is not a desktop thing: the utilities show as they are.
  const deskMore = await pp.evaluate(() => ({
    toggle: document.querySelector("#side-more-toggle").offsetParent !== null,
    hidden: [...document.querySelectorAll(".side-foot .chip")].filter((n) => n.offsetParent === null).length,
  }));
  check(!deskMore.toggle && deskMore.hidden === 0, `desktop: no More row, every utility shown (${JSON.stringify(deskMore)})`);
  // Nor is folding Needs you out of the workspaces: the blocked agent is
  // listed under its workspace too.
  const deskListed = await pp.evaluate((id) => ({
    needs: !!document.querySelector(`#attention .attn-row[data-id="${id}"]`),
    listed: !!document.querySelector(`#workspaces .session[data-id="${id}"]`),
    lines: document.querySelectorAll("#workspaces .ws-needs").length,
  }), agent.id);
  check(deskListed.needs && deskListed.listed && deskListed.lines === 0, `desktop: a blocked agent stays in its workspace's list (${JSON.stringify(deskListed)})`);
  // A long session name never squeezes the path or the usage button to
  // nothing: both keep a readable, clickable width.
  const meters = await pp.evaluate(() => ["#meter-cwd", "#meter-usage", ".crumb-session"].map((sel) => {
    const n = document.querySelector(sel);
    return { sel, hidden: !n || n.hidden, width: n ? Math.round(n.getBoundingClientRect().width) : 0 };
  }));
  check(meters.every((m) => !m.hidden && m.width >= 80), `desktop: a long session name leaves the path and usage readable (${JSON.stringify(meters)})`);
  // A long folder name in the sidebar ends in an ellipsis inside it.
  const folder = await pp.evaluate(() => {
    const side = document.querySelector("#sidebar").getBoundingClientRect();
    const n = [...document.querySelectorAll(".ws-repos span.git")].find((x) => x.textContent.startsWith("a-really"));
    if (!n) return { ok: false, why: "missing" };
    const r = n.getBoundingClientRect();
    const cs = getComputedStyle(n);
    return { ok: r.right <= side.right && cs.textOverflow === "ellipsis" && n.scrollWidth > n.clientWidth, right: r.right, side: side.right };
  });
  check(folder.ok, `desktop: a long sidebar folder name is ellipsized inside the sidebar (${JSON.stringify(folder)})`);
  // why? and usage open under their buttons.
  const anchored = async (trigger, pop) => {
    await pp.locator(trigger).click();
    await pp.waitForTimeout(400);
    const t = await rectOf(trigger);
    const p = await rectOf(pop);
    await pp.keyboard.press("Escape");
    await pp.waitForTimeout(200);
    const ok = !!(t && p) && p.top >= t.bottom && p.top - t.bottom <= 12 && p.left <= t.left + t.width / 2 && p.right >= t.left + t.width / 2 && p.right <= 1440;
    return { ok, t, p };
  };
  for (const [trigger, pop] of [["#crumb-why", "#why-pop"], ["#meter-usage", "#usage-pop"]]) {
    const a = await anchored(trigger, pop);
    check(a.ok, `desktop: ${pop} opens anchored under ${trigger} (${JSON.stringify(a)})`);
  }
  // A long worktree path is cut from its start: its end (the task) shows.
  const tail = (sel) => pp.evaluate(({ sel, task }) => {
    const n = document.querySelector(sel);
    if (!n) return { ok: false, why: "missing" };
    const text = n.firstChild;
    const range = document.createRange();
    const box = n.getBoundingClientRect();
    const at = (i) => { range.setStart(text, i); range.setEnd(text, i + 1); return range.getBoundingClientRect(); };
    const end = text.data.lastIndexOf(task.slice(-1));
    const first = at(1);
    const last = at(end);
    return {
      // The text is the path alone (copying it copies no marks).
      ok: !/[\u200e\u200f]/.test(n.textContent) && n.scrollWidth > n.clientWidth && getComputedStyle(n).textOverflow === "ellipsis" && last.right <= box.right + 1 && last.left >= box.left && first.left < box.left,
      text: text.data.trim().slice(-30),
    };
  }, { sel, task });
  const meterTail = await tail("#meter-cwd");
  check(meterTail.ok, `desktop: the header's worktree path is cut from its start (${JSON.stringify(meterTail)})`);
  await pp.locator(`.session[data-id="${agent.id}"]`).hover();
  await pp.locator(`.session[data-id="${agent.id}"] [aria-label="Close session"]`).click();
  await pp.waitForSelector("#dlg-close-wt[open] #cwt-where .path-tail", { timeout: 5000 });
  await pp.waitForTimeout(300);
  const dialogTail = await tail("#cwt-where .path-tail");
  check(dialogTail.ok, `desktop: the close dialog's worktree path is cut from its start (${JSON.stringify(dialogTail)})`);
  await pp.screenshot({ path: path.join(out, "desktop-close-worktree.png") });
  await pp.keyboard.press("Escape");
  await pp.waitForTimeout(300);
  // A toast while the fan-out board is open: above the board, clickable,
  // and never over it.
  await pp.locator('[data-action="fanouts"]').click();
  await pp.waitForSelector("#dlg-fanout[open] .fo-card, #dlg-fanout[open] button", { timeout: 10000 });
  await pp.locator("#dlg-fanout button", { hasText: "Re-grade" }).first().click();
  await pp.waitForSelector(".toast", { timeout: 5000 });
  await pp.waitForTimeout(400);
  const overBoard = async (dialog) => pp.evaluate((dialog) => {
    const d = document.querySelector(dialog).getBoundingClientRect();
    const bad = [];
    for (const t of document.querySelectorAll(".toast")) {
      const r = t.getBoundingClientRect();
      const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      if (!hit || !t.contains(hit)) bad.push(`under ${hit ? hit.tagName + "." + hit.className : "nothing"}`);
      if (r.left < d.right && r.right > d.left && r.top < d.bottom && r.bottom > d.top) bad.push("overlaps the dialog");
    }
    return { toasts: document.querySelectorAll(".toast").length, bad };
  }, dialog);
  const board = await overBoard("#dlg-fanout");
  check(board.toasts > 0 && board.bad.length === 0, `desktop: a toast over the fan-out board is on top, clickable, never over it (${JSON.stringify(board)})`);
  await pp.screenshot({ path: path.join(out, "desktop-board-toast.png") });
  await pp.keyboard.press("Escape");
  await pp.waitForTimeout(300);
  // ... and over a dialog opened while it shows.
  await pp.locator('.side-actions [data-action="new-session"]').click();
  await pp.waitForSelector("#dlg-session[open]");
  await pp.waitForTimeout(300);
  const sheet = await overBoard("#dlg-session");
  check(sheet.toasts > 0 && sheet.bad.length === 0, `desktop: a toast stays on top of a dialog opened after it, never over it (${JSON.stringify(sheet)})`);
  // Toasts off to the side of a centred dialog leave it where it is.
  const still = await pp.evaluate(async () => {
    const d = document.querySelector("#dlg-session");
    const before = d.getBoundingClientRect();
    const root = document.querySelector("#toasts");
    for (const n of [...root.children]) n.remove();
    await new Promise((r) => setTimeout(r, 200));
    const empty = d.getBoundingClientRect();
    // The app's own toasts (app-base.js's `commands`), so they are shown
    // the way every toast is.
    for (let i = 0; i < 3; i++) commands.toast(`side toast ${i}`, { timeout: 0 });
    await new Promise((r) => setTimeout(r, 300));
    const after = d.getBoundingClientRect();
    const sideways = [...root.children].every((t) => t.getBoundingClientRect().left >= after.right);
    for (const n of [...root.children]) n.remove();
    return { sideways, before: [before.top, before.bottom], empty: [empty.top, empty.bottom], after: [after.top, after.bottom] };
  });
  check(still.sideways && (still.after[0] === still.empty[0] && still.after[1] === still.empty[1]), `desktop: toasts beside a dialog do not move it (${JSON.stringify(still)})`);
  await pp.keyboard.press("Escape");
  await pp.waitForTimeout(300);
  await drawerChecks(pp, "desktop", false);
  await pol.close();
  // A finished turn is seen by looking at it, even after a long break
  // with no input (a session link opened, not a click): the page's idle
  // clock jumps past the 5-minute away mark, then the hash attaches it.
  const wsSeen = await api("POST", "/api/workspaces", { name: "seen", folders: [repo3] });
  const finished = await api("POST", "/api/sessions", { workspace_id: wsSeen.id, kind: "claude", prompt: "do it" });
  for (let i = 0; i < 100 && (await api("GET", `/api/sessions/${finished.id}`)).agent?.status !== "done"; i++) await sleep(100);
  const away = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const ap = await away.newPage();
  await ap.clock.install();
  await ap.goto(`${base}/login?token=${token}#session=${agent.id}`);
  await ap.waitForSelector("#terminal .pane .xterm", { timeout: 15000 });
  const unseen = (await api("GET", `/api/sessions/${finished.id}`)).agent?.status;
  await ap.clock.fastForward("06:00");
  await ap.evaluate((id) => { location.hash = `#session=${id}`; }, finished.id);
  let seenAs = unseen;
  for (let i = 0; i < 50 && seenAs === "done"; i++) {
    await sleep(100);
    seenAs = (await api("GET", `/api/sessions/${finished.id}`)).agent?.status;
  }
  check(unseen === "done" && seenAs === "idle", `desktop: a done turn opened by link after 6 idle minutes is seen (${unseen} -> ${seenAs})`);
  await away.close();

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
  // Claude's folder-trust dialog (captured at a phone's 55 columns) is an
  // unnumbered cursor menu with "No, exit" marked: Enter alone would exit,
  // so every option is offered, reached by arrow keys.
  const trustScreen = fs.readFileSync(path.join(root, "crates/flywheel-hub/detect/fixtures/claude-trust-narrow.txt"), "utf8").split("\n");
  const trust = await mp.evaluate((lines) => window.FwHubPrompt.parse(lines), trustScreen);
  check(
    trust && trust.options.length === 2 && trust.options[0].label === "❯ No, exit" && trust.options[0].seq === "\r" && trust.options[1].label === "Yes, I trust this folder" && trust.options[1].seq === "\x1b[B\r",
    `phone: the trust dialog's quick answers are its menu, reached by arrows (${JSON.stringify(trust)})`,
  );
  check(
    trust && trust.question === "Quick safety check: Is this a project you created or one you trust?",
    `phone: the trust dialog's question is its question, not the link below it (${JSON.stringify(trust && trust.question)})`,
  );
  // AskUserQuestion's rule above "Chat about this" is not part of the
  // option before it (captured at a desktop's width and a phone's).
  for (const name of ["claude-ask-question", "claude-ask-question-narrow"]) {
    const lines = fs.readFileSync(path.join(root, `crates/flywheel-hub/detect/fixtures/${name}.txt`), "utf8").split("\n");
    const asked = await mp.evaluate((l) => window.FwHubPrompt.parse(l), lines);
    const typed = asked && asked.options.find((o) => /Type something/.test(o.label));
    check(typed && /^\d+ {2}Type something\.$/.test(typed.label) && asked.options.every((o) => !/[─━]/.test(o.label)),
      `phone: ${name}: no rule in the quick answers (${JSON.stringify(asked && asked.options.map((o) => o.label))})`);
  }
  // An option's description is not part of its label (a real phone showed
  // "2  Green Green"); a wrapped label is (Codex's long "don't ask again").
  const described = await mp.evaluate((l) => window.FwHubPrompt.parse(l),
    fs.readFileSync(path.join(root, "crates/flywheel-hub/detect/fixtures/claude-ask-question-narrow.txt"), "utf8").split("\n"));
  check(described && described.options[0].label === "1  Blue (Recommended)",
    `phone: a question's option label is its label (${JSON.stringify(described && described.options[0])})`);
  check(described && described.options[0].detail === "Neutral and widely readable, a common safe default for UI accents." && described.options[3].detail === "",
    `phone: a question's option description is its detail (${JSON.stringify(described && described.options.map((o) => o.detail))})`);
  // The real 55-column screen: option 1's description wraps, option 2's
  // label wraps; read apart from the screen alone.
  const realWrapped = fs.readFileSync(path.join(root, "crates/flywheel-hub/tests/fixtures/claude-ask-question-wrapped.txt"), "utf8").split("\n");
  const fromScreen = await mp.evaluate((l) => window.FwHubPrompt.parse(l), realWrapped);
  check(fromScreen && fromScreen.options[0].label === "1  Red" && fromScreen.options[0].detail === "A warm color that stands out on dark and light backgrounds alike"
    && fromScreen.options[1].label === "2  A calm and deep ocean-like blue that suits documentation headings well" && fromScreen.options[1].detail === "Cool and quiet",
    `phone: a real wrapped label and wrapped description stay apart (${JSON.stringify(fromScreen && fromScreen.options.slice(0, 3))})`);
  // A whole label the screen cannot tell from a wrapped one (its
  // description's first word would not have fit after it): the agent's
  // own options (its status's questions) keep them apart.
  const ambiguous = ["Which config?", "", "❯ 1. Use the existing configuration file in the repository root",
    "     Supercalifragilistic defaults stay as they are", "  2. Start fresh", "     Writes a new file", "  3. Type something.",
    "──────────────────────────────────────────────────────────────────────", "  4. Chat about this", "", "Enter to select · ↑/↓ to navigate · Esc to cancel"];
  const questions = [{ question: "Which config should the setup use?", options: [
    { label: "Use the existing configuration file in the repository root", description: "Supercalifragilistic defaults stay as they are" },
    { label: "Start fresh", description: "Writes a new file" }] }];
  const guessed = await mp.evaluate((l) => window.FwHubPrompt.parse(l), ambiguous);
  const told = await mp.evaluate(([l, q]) => window.FwHubPrompt.parse(l, q), [ambiguous, questions]);
  check(guessed && guessed.options[0].detail === "", `phone: the screen alone takes that description for more label (${JSON.stringify(guessed && guessed.options[0])})`);
  check(told && told.options[0].label === "1  Use the existing configuration file in the repository root" && told.options[0].detail === "Supercalifragilistic defaults stay as they are"
    && told.options[1].detail === "Writes a new file" && told.options[2].label === "3  Type something." && told.question === "Which config should the setup use?",
    `phone: the agent's own options keep label and description apart (${JSON.stringify(told)})`);
  const wrappedLines = ["Would you like to run the following command?", "", "  $ cargo test -p parser", "",
    "› 1. Yes, proceed (y)", "  2. Yes, and don't ask again for commands that start with", "     `cargo test -p parser` (p)",
    "  3. No, and tell Codex what to do differently (esc)", "", "  Press enter to confirm or esc to cancel"];
  const wrappedAsk = await mp.evaluate((l) => window.FwHubPrompt.parse(l), wrappedLines);
  check(wrappedAsk && wrappedAsk.options[1].label === "2  Yes, and don't ask again for commands that start with `cargo test -p parser`" && wrappedAsk.options[1].seq === "p" && !wrappedAsk.options[1].detail,
    `phone: a wrapped option label stays whole (${JSON.stringify(wrappedAsk && wrappedAsk.options[1])})`);
  // "Always allow" is offered for every tool permission or approval, as
  // its status detail reads it (screen-read details do not say
  // "permission"), and for no trust dialog, question, or MCP form.
  const fixture = (name) => fs.readFileSync(path.join(root, `crates/flywheel-hub/detect/fixtures/${name}.txt`), "utf8").split("\n");
  const allowCases = [
    ["codex-approval-exec", "Would you like to run the following command? $ cargo test -p parser", true],
    ["codex-approval-patch", "Would you like to make the following edits? Destination: /home/me/code/app/src/parser.rs", true],
    ["codex-approval-mcp", "github needs your approval.", true],
    ["codex-approval-network", "Do you want to approve network access to example.com?", true],
    ["claude-permission-write", "Do you want to create notes.txt?", true],
    ["claude-permission-bash", "permission to use Bash", true],
    ["claude-trust-narrow", "asks whether to trust this folder", false],
    ["claude-ask-question", "Do you prefer red or blue?", false],
    ["codex-question", "asks a question", false],
    ["codex-trust", "asks whether to trust this folder", false],
    ["codex-mcp-form", "Approve the pending action.", false],
  ];
  const allowed = await mp.evaluate((cases) => cases.map(([name, detail, lines]) => [name, window.FwHubPrompt.asksPermission(detail, lines)]),
    allowCases.map(([name, detail]) => [name, detail, fixture(name)]));
  const wrong = allowed.filter(([name, got]) => got !== allowCases.find((c) => c[0] === name)[2]);
  check(!wrong.length, `phone: Always allow is offered for permission prompts only (${JSON.stringify(wrong)})`);
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
  // The first key row fits the narrowest phones: no key past the row's
  // edge (nor the screen's), every key 44x44; and the quick answers (shown
  // here with a stand-in prompt) are 44px targets, keyboard up or down.
  for (const width of [360, 375, 390, 430]) {
    await mp.setViewportSize({ width, height: 844 });
    await mp.waitForTimeout(300);
    const fit = await mp.evaluate(() => {
      const bad = [];
      const row = document.querySelector("#keybar .kb-primary");
      const box = row.getBoundingClientRect();
      if (row.scrollWidth > row.clientWidth + 0.5) bad.push(`row scrolls ${row.scrollWidth} > ${row.clientWidth}`);
      if (box.right > innerWidth + 0.5) bad.push(`row ends at ${Math.round(box.right)} past ${innerWidth}`);
      for (const key of row.querySelectorAll(".kb-key")) {
        const r = key.getBoundingClientRect();
        const name = key.textContent.trim();
        if (r.left < box.left - 0.5 || r.right > box.right + 0.5) bad.push(`${name} outside the row (${Math.round(r.left)}..${Math.round(r.right)})`);
        if (r.width < 44 || r.height < 44) bad.push(`${name} ${r.width.toFixed(1)}x${r.height.toFixed(1)}`);
      }
      const quick = document.querySelector("#quick");
      const was = { hidden: quick.hidden, html: quick.innerHTML };
      quick.hidden = false;
      quick.innerHTML = '<div class="qa-head"><div class="qa-question"><b>blocked</b> Run rm?</div><button type="button" class="qa-esc">esc</button></div>'
        + '<div class="qa-options"><button type="button" class="qa">1. Yes</button><button type="button" class="qa">2. Yes, and don\'t ask again</button><button type="button" class="qa">3. No</button></div>';
      for (const open of [false, true]) {
        document.body.classList.toggle("kb-open", open);
        for (const node of quick.querySelectorAll(".qa, .qa-esc")) {
          if (node.offsetParent === null) continue;
          const r = node.getBoundingClientRect();
          if (r.width < 44 || r.height < 44) bad.push(`${open ? "kb-open " : ""}${node.className} ${r.width.toFixed(1)}x${r.height.toFixed(1)}`);
        }
      }
      document.body.classList.remove("kb-open");
      quick.innerHTML = was.html;
      quick.hidden = was.hidden;
      return bad;
    });
    check(fit.length === 0, `phone ${width}: the first key row fits, keys and quick answers at least 44x44 (${fit.join(", ")})`);
    if (width === 360) await mp.screenshot({ path: path.join(out, "phone-360-keys.png") });
  }
  await mp.setViewportSize({ width: 390, height: 844 });
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
  // Needs you is right above the workspaces: a session it lists is not
  // listed again, its workspace says "N in Needs you" (a 44px row).
  const twice = await mp.evaluate((id) => {
    const needs = [...document.querySelectorAll("#attention .attn-row[data-id]")].map((r) => r.dataset.id);
    return {
      needs: needs.includes(id),
      again: needs.filter((n) => document.querySelector(`#workspaces .session[data-id="${n}"]`)),
      lines: [...document.querySelectorAll("#workspaces .ws-needs")].map((n) => [n.textContent, Math.round(n.getBoundingClientRect().height)]),
    };
  }, agent.id);
  check(
    twice.needs && twice.again.length === 0 && twice.lines.some(([t]) => t === "1 in Needs you") && twice.lines.every(([, h]) => h >= 44),
    `phone: a session in Needs you is one "N in Needs you" line under its workspace (${JSON.stringify(twice)})`,
  );
  // The footer's utilities are one More row (44px) ...
  const folded = await mp.evaluate(() => ({
    toggle: Math.round(document.querySelector("#side-more-toggle").getBoundingClientRect().height),
    shown: [...document.querySelectorAll(".side-foot .chip")].filter((n) => n.offsetParent !== null).length,
  }));
  check(folded.toggle >= 44 && folded.shown === 0, `phone: the footer is one More row (${JSON.stringify(folded)})`);
  // ... unfolding into rows, each whole, uncovered and at least 44px.
  await mp.locator("#side-more-toggle").tap();
  await mp.waitForTimeout(500);
  const unfolded = await mp.evaluate(() => {
    const bad = [];
    const chips = [...document.querySelectorAll(".side-foot .chip")];
    for (const n of chips) {
      if (n.offsetParent === null) { bad.push(`${n.id || n.dataset.action} hidden`); continue; }
      n.scrollIntoView({ block: "center" });
      const r = n.getBoundingClientRect();
      const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      if (!hit || !n.contains(hit)) bad.push(`${n.id || n.dataset.action} covered`);
      if (r.height < 44 || r.width < 44) bad.push(`${n.id || n.dataset.action} ${Math.round(r.width)}x${Math.round(r.height)}`);
      if (!n.textContent.trim()) bad.push(`${n.id || n.dataset.action} has no name`);
    }
    return { count: chips.length, expanded: document.querySelector("#side-more-toggle").getAttribute("aria-expanded"), bad };
  });
  check(unfolded.count >= 9 && unfolded.expanded === "true" && unfolded.bad.length === 0, `phone: More unfolds every utility as a 44px row (${JSON.stringify(unfolded)})`);
  await mp.screenshot({ path: path.join(out, "phone-sidebar-more.png") });
  // Picking one folds them again.
  await mp.locator('.side-foot [data-action="activity"]').tap();
  await mp.waitForTimeout(400);
  const refolded = await mp.evaluate(() => document.querySelector("#side-more-toggle").getAttribute("aria-expanded"));
  check(refolded === "false", `phone: picking a utility folds More again (${refolded})`);
  await mp.keyboard.press("Escape");
  await mp.waitForTimeout(300);
  // More agents blocked than Needs you's short scroll shows: three in one
  // workspace, then one in another (blocked last, so listed last, below
  // the fold). The list says there is more, and that workspace's
  // "1 in Needs you" line brings its agent into view.
  const crowded = [];
  for (const [name, n] of [["crowd", 3], ["stray", 1]]) {
    const dir = path.join(tmp, "code", name);
    fs.mkdirSync(path.join(dir, ".git"), { recursive: true });
    const ws = await api("POST", "/api/workspaces", { name, folders: [dir] });
    for (let i = 0; i < n; i++) {
      crowded.push(await api("POST", "/api/sessions", { workspace_id: ws.id, kind: "claude", prompt: "polish", name: `${name}-${i}`, cols: 80, rows: 24 }));
      await sleep(300);
    }
  }
  const stray = crowded[crowded.length - 1];
  if (!(await mp.evaluate(() => document.body.classList.contains("side-open")))) await mp.locator(".menu-toggle").tap();
  const seen = (id) => mp.evaluate((id) => {
    const row = document.querySelector(`#attention .attn-row[data-id="${id}"]`);
    if (!row) return { row: false };
    const r = row.getBoundingClientRect();
    const hit = document.elementFromPoint(r.left + r.width / 2, r.top + Math.min(r.height / 2, 10));
    return { row: true, visible: !!hit && row.contains(hit), more: document.querySelector("#sidebar .attn-list").classList.contains("more-below") };
  }, id);
  await mp.waitForFunction((id) => !!document.querySelector(`#attention .attn-row[data-id="${id}"]`), stray.id, { timeout: 15000 }).catch(() => {});
  await mp.evaluate(() => { document.querySelector("#sidebar").scrollTop = 0; });
  await mp.waitForTimeout(300);
  const before = await seen(stray.id);
  check(before.row && !before.visible && before.more, `phone: a capped Needs you says there is more below (${JSON.stringify(before)})`);
  const line = mp.locator(`#workspaces section.workspace:has(.ws-name:text-is("stray")) .ws-needs`).first();
  await line.scrollIntoViewIfNeeded();
  await line.tap();
  await mp.waitForTimeout(900);
  const after = await seen(stray.id);
  check(after.visible, `phone: "1 in Needs you" brings that workspace's agent into view (${JSON.stringify(after)})`);
  await mp.screenshot({ path: path.join(out, "phone-needs-reveal.png") });
  for (const s of crowded) await api("DELETE", `/api/sessions/${s.id}`);
  await mp.waitForTimeout(500);
  // Drawers on a phone, over an agent in a real repo (Review needs git).
  await mp.goto(`${base}/login?token=${token}#session=${agent.id}`);
  await mp.waitForSelector("#terminal .pane .xterm", { timeout: 15000 });
  await mp.waitForTimeout(800);
  await drawerChecks(mp, "phone", true);
  await glyphChecks(mp, "phone");
  // The replay player and a share link's view: every control at least
  // 44x44 (a checkbox counts by its label).
  await api("POST", `/api/sessions/${sids[1]}/record/start`);
  await api("POST", `/api/sessions/${sids[1]}/input`, { data: "echo recorded\r" });
  await sleep(800);
  const recording = (await api("POST", `/api/sessions/${sids[1]}/record/stop`)).recording;
  const share = await api("POST", "/api/shares", { session_id: sids[1], expires_in: 600 });
  const tapTargets = (page) => page.evaluate(() => [
    ...document.querySelectorAll("button, a[href], input, select, summary, [role=button]"),
  ].filter((n) => n.offsetParent !== null || getComputedStyle(n).position === "fixed")
    .map((n) => {
      const target = n.closest("label") || n;
      const r = target.getBoundingClientRect();
      return [n.id || n.className || n.tagName, Math.round(r.width), Math.round(r.height)];
    })
    .filter(([, w, h]) => w > 0 && (w < 44 || h < 44)));
  await mp.goto(`${base}/replay#${encodeURIComponent(recording.name)}`);
  await mp.waitForSelector(".xterm", { timeout: 15000 });
  await mp.waitForTimeout(800);
  await glyphChecks(mp, "phone /replay");
  await exactTextChecks(mp, "phone /replay", [".xterm-rows"]);
  const replaySmall = await tapTargets(mp);
  check(replaySmall.length === 0, `phone: /replay tap targets at least 44x44 (${JSON.stringify(replaySmall)})`);
  await mp.screenshot({ path: path.join(out, "phone-replay.png") });
  const viewer = await phone.newPage();
  await viewer.goto(`${base}/share/${share.credential}`);
  await viewer.waitForSelector(".xterm", { timeout: 15000 });
  await viewer.waitForTimeout(800);
  await glyphChecks(viewer, "phone /share");
  await exactTextChecks(viewer, "phone /share", [".xterm-rows"]);
  const shareSmall = await tapTargets(viewer);
  check(shareSmall.length === 0, `phone: /share tap targets at least 44x44 (${JSON.stringify(shareSmall)})`);
  await viewer.screenshot({ path: path.join(out, "phone-share.png") });
  await phone.close();

  // --- seen on view: a finished turn someone is looking at is seen -------
  // On a page that is visible and focused, attached to the agent: a turn
  // that ends after minutes without input (someone watched it work) is
  // seen at once; one that started while nobody was here stays done until
  // someone is back; one watched past WATCH_MS (30 min) counts as away.
  const watch = await api("POST", "/api/sessions", { workspace_id: ws.id, kind: "claude", name: "watch", cols: 100, rows: 30 });
  const seenCtx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const seenPage = await seenCtx.newPage();
  await seenPage.clock.install();
  await seenPage.goto(`${base}/login?token=${token}`);
  await seenPage.waitForURL(`${base}/`);
  await seenPage.goto(`${base}/#session=${watch.id}`);
  await seenPage.waitForSelector("#terminal .pane.focused .xterm", { timeout: 15000 });
  await seenPage.waitForTimeout(1500);
  const say = (text) => execFileSync(fw, ["hub", "send", "--state-dir", state, watch.id, `${text}\n`]);
  const statusOf = async () => ((await api("GET", `/api/sessions/${watch.id}`)).agent || {}).status;
  const until = async (want) => {
    for (let i = 0; i < 100; i++) {
      const now = await statusOf();
      if (now === want) return now;
      await sleep(50);
    }
    return statusOf();
  };
  say("e2e-go");
  await until("working");
  await seenPage.clock.fastForward("06:00");
  await seenPage.waitForTimeout(500);
  say("e2e-stop");
  let got = await until("idle");
  check(got === "idle", `seen: a turn watched for 6 quiet minutes is seen when it ends (${got})`);
  await seenPage.clock.fastForward("06:00");
  await seenPage.waitForTimeout(500);
  say("e2e-go");
  await until("working");
  await seenPage.waitForTimeout(500);
  say("e2e-stop");
  got = await until("done");
  await seenPage.waitForTimeout(500);
  got = await statusOf();
  check(got === "done", `seen: a turn that started and ended while nobody was here stays done (${got})`);
  await seenPage.mouse.move(700, 400);
  await seenPage.mouse.move(720, 420);
  got = await until("idle");
  check(got === "idle", `seen: someone back at the page sees it (${got})`);
  say("e2e-go");
  await until("working");
  await seenPage.waitForTimeout(500);
  await seenPage.clock.fastForward("31:00");
  await seenPage.waitForTimeout(500);
  say("e2e-stop");
  await until("done");
  await seenPage.waitForTimeout(500);
  got = await statusOf();
  check(got === "done", `seen: a turn watched past 30 quiet minutes counts as away (${got})`);
  await seenPage.evaluate(() => window.dispatchEvent(new Event("focus")));
  got = await until("idle");
  check(got === "idle", `seen: focusing the window counts as back (${got})`);

  // Transcript viewer: tool results are collapsible blocks under their
  // calls (named for the call's tool): a long Bash output shows its first
  // lines until expanded, a question's answers whole.
  const conversation = (await api("GET", `/api/sessions/${watch.id}`)).agent_session_id;
  const bashOut = Array.from({ length: 20 }, (_, i) => `line ${i + 1}`).join("\n");
  const tline = (o) => JSON.stringify({ sessionId: conversation, timestamp: "2026-11-04T10:00:00.000Z", ...o });
  fs.writeFileSync(path.join(claudeConfig, "projects", "e2e", `${conversation}.jsonl`), [
    tline({ type: "user", uuid: "t-u1", message: { role: "user", content: "run it" } }),
    tline({ type: "assistant", uuid: "t-a1", message: { id: "m-t1", role: "assistant", content: [{ type: "tool_use", id: "toolu_b", name: "Bash", input: { command: "seq 20" } }] } }),
    tline({ type: "user", uuid: "t-u2", message: { role: "user", content: [{ type: "tool_result", tool_use_id: "toolu_b", content: bashOut }] } }),
    tline({ type: "assistant", uuid: "t-a2", message: { id: "m-t2", role: "assistant", content: [{ type: "tool_use", id: "toolu_q", name: "AskUserQuestion", input: { questions: [{ question: "Which color?", options: [{ label: "Red", description: "Warm" }, { label: "Blue", description: "Calm" }] }] } }] } }),
    tline({ type: "user", uuid: "t-u3", message: { role: "user", content: [{ type: "tool_result", tool_use_id: "toolu_q", content: 'User has answered your questions: "Which color?"="Red". You can now continue with the user\'s answers in mind.' }] } }),
    tline({ type: "assistant", uuid: "t-a3", message: { id: "m-t3", role: "assistant", content: [{ type: "text", text: "Red it is." }] } }),
  ].join("\n") + "\n");
  // The hub finds a new conversation's file on its next look.
  for (let i = 0; i < 100 && !(await api("GET", `/api/sessions/${watch.id}/transcript?before=1&after=1`)).transcript; i++) await sleep(100);
  await seenPage.evaluate((id) => search.openTranscript({ session_id: id, session: "watch", workspace: "e2e", kind: "claude", state: "running" }, 0), watch.id);
  await seenPage.waitForSelector("#tx-body .tx-msg.kind-result", { timeout: 10000 });
  const results = () => seenPage.evaluate(() => [...document.querySelectorAll("#tx-body .tx-msg.kind-result")].map((a) => ({
    who: a.querySelector(".tx-who").textContent,
    open: a.querySelector("details.tx-result").open,
    lines: a.querySelector(".tx-result pre").textContent.split("\n").length,
    more: (a.querySelector(".tx-expand") || {}).textContent || "",
  })));
  let shownResults = await results();
  check(shownResults.length === 2 && shownResults[0].who === "Bash result" && shownResults[0].open && shownResults[0].lines === 8 && shownResults[0].more === "Show all 20 lines",
    `transcript: a long Bash output is a block cut to 8 lines with an expand (${JSON.stringify(shownResults)})`);
  check(shownResults[1] && shownResults[1].who === "AskUserQuestion result" && shownResults[1].lines === 1 && !shownResults[1].more,
    `transcript: a question's answers show whole (${JSON.stringify(shownResults[1])})`);
  await seenPage.locator("#tx-body .tx-expand").first().click();
  shownResults = await results();
  check(shownResults[0].lines === 20 && shownResults[0].more === "Show less", `transcript: expanded, the whole output (${JSON.stringify(shownResults[0])})`);
  await seenPage.locator("#tx-body .tx-result > summary").first().click();
  shownResults = await results();
  check(!shownResults[0].open, "transcript: a result block collapses");
  await seenPage.screenshot({ path: path.join(out, "desktop-transcript-results.png") });
  await seenCtx.close();
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

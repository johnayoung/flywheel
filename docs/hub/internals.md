# Hub internals

How the hub behaves underneath the UI. Front door: [hub](../hub.md).

## Model

- **Workspace**: unique name + 1..32 **folders** (existing dirs; git roots or plain), optionally labelled. A **missing** folder refuses new sessions.
- **Session**: one PTY in one folder: `shell` (`$SHELL` or an argv), `claude`, or `codex` (on the hub's `PATH`, else 400). `running`, `exited` (code or signal; scrollback stays), or `stopped` (hub stopped under it; `last_cwd`, `restore_error`).
- `state.json`: atomic writes, schema 5; older migrate, newer are refused.
- Kill hangs up the leader's group and background jobs (Linux scans `/proc`).
- One shared terminal size (max 500x300): a web pane resizes the PTY only while focused or sole viewer; others clip. Input queued past 4 MiB for a terminal not reading is dropped (`nack` + `error`).
- Env: [hub-reference](reference.md#environment); enclosing-agent markers (`CLAUDECODE`, `CLAUDE_CODE_*`, `AI_AGENT`, `CODEX_SANDBOX`, ...) are removed.

## Terminal emulation

- One `alacritty_terminal` emulator per session; attach = a **snapshot** (scrollback, screens, cursor, pen, modes, title, palette), then live bytes, no gap or overlap. Snapshots hold 2 MiB of scrollback; a client behind gets a screen-only one (`reset`; protocol 2: a frame).
- Weak links ([hub-api](api.md#terminal-websocket)): 512 KiB of recent output per session (kept across `upgrade`) lets a dropped viewer resume; row diffs pace slow ones. A relaunch closes the old process's sockets (4005).
- Not replayed: custom tab stops, blink/overline. Not emulated: `?47`/`?1047`, kitty keyboard.
- The server answers terminal queries (DA, DSR, DECRQM, OSC 4/10/11/12) once; clients must not. Colours follow the last viewer-reported theme (dark first).
- Bounds: OSC strings 64 KiB, titles 4 KiB (stack 10), hyperlinks over 2 KiB dropped, 8 combining marks per cell.

## Lifecycle and restore

- `start` re-execs `fw hub serve` detached into `hub.log` (10 MiB, 3 kept); ready = `server.json`, else SIGTERM at `--timeout` with the log tail. `stop` SIGTERMs and waits for the lock; `--force` SIGKILLs (no restore record).
- One hub per state dir (`serve.lock`); a stale `server.json` is removed, so the CLI never sends the token to whatever listens there now.
- Shutdown records running sessions `stopped`, then SIGHUP, then SIGKILL (`nohup`ed children too); SIGTERM during startup waits until serving.

**Restore** (each start unless `--no-restore`, which drops pending ones for good) of `stopped` sessions: same id, name, order; fresh PTY.

| Recorded as | Comes back as |
|---|---|
| interactive shell (`bash`, `zsh`, ... at most `-l`/`-i`) | same shell in its last cwd (Linux `/proc`), else its folder |
| claude with a transcript | `claude --resume <agent_session_id>`, fresh hooks |
| codex with a rollout for its thread id | `codex -c notify=[...] resume <thread-id>` |
| anything else, or agents under `--no-agent-resume` | a fresh shell (`fallback_command`; never re-runs the command) |
| folder gone | stays `stopped` with `restore_error`, retried each start |
| found `running` (crash) | not restored ("not shut down cleanly"); `fw hub relaunch` |

Decision: `restore::plan`. `install-service` writes `~/.config/systemd/user/<name>.service` (`KillMode=mixed`, `--no-login-code`) and prints enable/linger steps.

## Live handoff (`fw hub upgrade`)

Replaces the hub process **without ending any session**: same pids, screens, scrollback, port, status, hook secrets. Linux only (macOS compiles, untested).

- Binary: `--binary`, else `fw` on `PATH`, else this one; it inherits the old hub's environment. Agents spawned earlier keep hooks on the old path: upgrade in place or keep it.
- Over `handoff.sock` (same user) the old hub freezes (503, PTYs paused), closes WebSockets with **1012**, passes listener, locks, PTY masters, pidfds (`SCM_RIGHTS`), snapshots, held input, input ids taken, the resume ring, and exits once the new hub acks. Input refused meanwhile is `nack`ed for a resend: never lost or doubled.
- Failure before commit thaws the old hub; a stuck new hub is killed at `--timeout`.
- Refused (exit 4, nothing changed): protocol skew, a pre-handoff hub, a binary without `--handoff`; `--allow-restart` then restarts (sessions restored, not kept). systemd: `systemctl --user restart <unit>`.
- Handed-over processes are not children: exits come via pidfd, code unknown; no restore pass. Clients reconnect on 1012 (browsers at once, `attach` / `wait` within 2 min).

## Agent status

| Status | Meaning | Claude Code | Codex |
|---|---|---|---|
| `working` | busy | `UserPromptSubmit`, `PreToolUse`, `PostToolUse[Failure]` | screen: status line (`Working (12s • esc to interrupt)`) |
| `blocked` | waits on a person | `PermissionRequest`; `Notification` permission / elicitation / `agent_needs_input`; screen: folder trust or a known dialog 3 s with no hook | screen: approvals, `request_user_input`, folder trust |
| `done` | finished a turn **this client** has not viewed | `Stop`, `StopFailure` | `notify` `agent-turn-complete` |
| `idle` | seen, or waiting for a first prompt | `SessionStart`; viewed `done`; an interrupted turn | viewed `done`; screen: the composer (`›`) |
| `unknown` | no signal, or agent ended | `SessionEnd`, exit | exit; status line frozen 5 s; no rule matches 3 s |

- **Idle is not authorization**: no running turn, no known prompt on screen. An unknown prompt reads idle or unknown, never blocked.
- Hook / `notify` reports win; screen rules fill gaps; shells have none. Esc fires no Claude hook: a seen turn showing the prompt 3 s reads `idle`, a dialog closing onto a running turn `working`.
- Seen is per client id; the REST API counts a turn seen once anyone viewed it.
- Rollup: a workspace's most urgent session, blocked > working > done > idle.
- Fan-out done (one rule for rollup, Needs you, chip, `fw hub attach`): until a winner is picked, candidates' finished turns roll up `idle`; an unseen grading (`needs_you`) is one `done` until anyone opens the board (`POST .../seen`).
- Claude gets `--session-id <uuid>` (`agent_session_id`, follows `/clear`) and hooks via `--settings` (merged with yours) running `<this binary> hub report`. Codex gets `-c notify=["<this binary>","hub","report","--source","codex","--event","notify"]`; its `thread-id` is `agent_session_id`. Trust: [hub-security](security.md#trust-model).

## Detection manifests

Per-kind TOML screen rules read against the **live bottom** of the screen: `crates/flywheel-hub/detect/{claude,codex,shell}.toml`, each replaced wholesale by `<state-dir>/agent-detection/<kind>.toml` (re-read within 1 s; an invalid one is logged and ignored).

```toml
kind = "codex"  # must match the file name
version = 2  # shown by explain
[[rule]]  # first rule whose every clause holds wins
id = "approval"
state = "blocked"  # working | blocked | idle | hold (keep the status)
detail = "asks for approval"
detail_line = ['(?i)^\s*(would you like to .+\?)$']  # capture 1 becomes the detail (wins over detail)
[[rule.when]]
lines = 1  # last N non-empty rows (1-100, default 30)
line = ['(?i)^\s*press enter to confirm or esc to cancel\s*$']
[[rule.when]]
lines = 100
any = ["would you like to run the following command?"]  # also all, none, not_line
```

- Pin `blocked` rules to a dialog's last-row footer so quoted text does not match. Limits: 64 rules, 8 clauses, 16 patterns per list, 400 chars, 64 KiB per file; read at most every 250 ms.
- Sources: Claude Code 2.1.306 screens (`detect/fixtures/`), `codex-rs/tui` strings; shell rules only answer `explain`.
- `fw hub explain S` (UI `why?`): status, source (`hook`, `manifest` + rule, `heuristic`, `hub`), a fresh screen read with evidence, why each rule missed.

## Terminal client: `fw hub attach`

Any terminal, local or over SSH (`--url`, `FW_HUB_TOKEN`).

- No SESSION: a picker (`j`/`k`, Enter, `/`, `q`). Attached: snapshot, then raw bytes; typing verbatim; the PTY follows this terminal; a relaunch elsewhere reattaches.
- Prefix `C-b` (`--prefix C-a`): `d` detach, `s` switch, `t` status bar, `r` redraw, `?` keys, twice sends it. The status bar shows `workspace/session [status]` and `N blocked  N done` (`--no-status` hides it).
- 256 KiB in flight at most (then a fresh snapshot); the terminal is restored on detach, error, signal, or panic. Token rotated: a state-dir token is re-read, a hand-given one exits 1.

## Layout: tabs and split panes

Per workspace: tabs of split trees, one session per pane. Stored on the hub (shared); active tab, focus, zoom per browser.

- Desktop: tab bar (`+`, double-click renames, middle-click closes). Drop a sidebar session on a pane edge to split, its middle to show it.
- A new-shell split (`Alt \`, `Alt -`) opens an agent's companion shell first; ``Alt ` `` opens it beside its agent above 1100 px.
- Panes stay 40x8 cells+ (else a new tab). Closing a pane (`Alt X`) leaves its session running; removing a session closes its panes and empty tabs. Only the active tab attaches; 12 tabs, 12 panes per tab.
- Phones (< 760 px): one pane, a chip row switches sessions; layouts unchanged.
- Browser checks (opt-in, temp hub, fake `claude`; env `FW`, `PLAYWRIGHT`, `CHROMIUM`, `PORT`): `node crates/flywheel-hub/e2e/layout.mjs [OUT_DIR]` (layouts, tap targets at 1440x900, 390x844); `e2e/network.mjs [--baseline] [--full]` (a 400/150 kbps, 300 ms proxy).

Keys: [hub-reference](reference.md#keyboard-shortcuts).

## Web UI

- Sidebar: machines > workspaces (rollup chip) > sessions (status dot, `↑ahead ↓behind ~changed ?untracked`); companions under their agent; blocked rows show the question. Folder chips: filled = git, hollow = plain, struck = missing.
- Title and favicon count blocked and done. `#session=<id>` survives reloads; a dropped or silent (no pong in 6 s) socket keeps the screen and reconnects (2 s for 30 s, then up to 10 s) with what it missed.
- Keys typed while down wait (`N queued`), sent in order once back; dropped, and said, if the session changed (epoch). No local echo; unacked 500 ms: `sending…`. Assets: gzipped, `?v=`-addressed, cached for good.
- Drawers (Commands, Flywheel, Review, Queue): one width, stage's right edge; phone: full-screen sheets (Flywheel closes on switching folders).
- Ended sessions: Relaunch, Close; relaunched elsewhere: reattaches; removed elsewhere: pane goes (toast).
- New workspace: paths or **Import VS Code workspace**. New session: kind, folder, worktree. First run: import offers, a 30 s tour.
- Themes: auto, dark, light (dim TUI colours lifted to 4.5:1).
- Embedded (licenses in `web/vendor/`): xterm.js 6.0.0 (+ fit, web-links, search); OFL fonts. Terminal: JetBrains Mono 2.304, then JuliaMono for its gaps (`tests/glyphs.rs`).

## Phone

Tested: 390x844, 430x932.

- Pinned to the visual viewport (the keyboard shrinks the terminal); the PTY keeps 16+ rows (shorter boxes clip around the cursor).
- **Needs you**: blocked and done agents of every workspace (blocked, longest waiting, first) head the sidebar (one scroll) and the start screen; a workspace lists them as one `N in Needs you` row (tap: scrolls to them). The footer's utilities are one **More** row.
- **Key bar** (touch): `esc` arrows `enter` `^c` (44px, fits 360px); then `tab` `shift-tab` sticky `ctrl` `alt`, `/ | ~ -`, `pgup` `pgdn`, text size, `kbd`, `compose`. Modifiers apply to key-bar keys too.
- **Quick answers**: a blocked dialog's options as buttons. **Compose**: multi-line text as one bracketed paste + Enter; per-device history, per-session drafts.
- Touch: swipe scrolls, long-press selects, pinch zooms.
- **Alerts** (per device): a notification when an agent turns blocked or done while the page is hidden or idle 5 min (https or localhost).
- **Lite mode** (Settings > Terminal, header `lite` pill; per device; `auto`: browser says slow, transfers under 64 KB/s, or 3+ round trips at 300 ms+): terminals at most 4 updates/s, hidden panes none; side panels wait (60 s); re-lists every 3 s at most.
- **Install**: PWA; the service worker never touches `/api` or sockets, and serves the cached page only when the hub is unreachable.

## Search

`Ctrl Shift F`, `fw hub search`, `GET /api/search`: one query across every session.

- Sources: `scrollback` (soft wraps joined, exited sessions too), `transcript` (current conversation: user/assistant text, tool calls; no thinking or tool output), `commands`.
- Literal or `regex` (linear-time, 1 MiB compiled), case-insensitive unless `case`, at most 1 KiB, never across lines. Newest first; 20 per session and source, 200 total; `offset` pages.
- Bounds: 1.5 s (then `partial`), 2 searches at once, transcript indexes 256 MiB (dropped after 10 min unused); 20 sessions x 5k lines: ~30 ms.
- The UI also asks every reachable machine (6 s each), naming offline ones; CLI `--machine`. Hits open at the match.

## Recording

```bash
fw hub record start agent  # or the header's rec pill
fw hub record stop agent; fw hub record ls
fw hub record get <name> -o demo.cast; asciinema play demo.cast  # or /replay#<name>
```

- Opt-in: `recordings/<session>-<UTC time>.cast`, asciinema v2 (`o`, `r` resizes, `m` markers like `exit: N`), from a screen snapshot.
- `recordings.part_cap` (64 MiB) rotates to `<base>.part<N>.cast`; `recordings.dir_cap` (1 GiB) deletes the oldest finished.
- Ends on stop, exit, relaunch, or hub stop; `fw hub upgrade` continues the file, a restart does not. This hub only (peers 403); an active one cannot be deleted (409).
- `/replay#<name>`: seek with marker ticks, 0.5-8x, idle gaps cut to 2 s, follows parts.

## Push notifications

ntfy-compatible POST when an agent becomes **blocked** or **done**, so a phone hears it with every browser closed.

```bash
fw hub start --notify-url https://ntfy.sh/<long-random-topic>  # prefer notify.toml: flags show in ps
```

- `<session> is blocked|done`, workspace / kind / branch, the blocked detail, `Priority: high` when blocked, `Click:` the session URL; never terminal content, paths, or tokens.
- Sent once the status held `settle_secs` (3 s), nobody views it, and a done turn is unseen; once per spell, `debounce_secs` per session and status.
- One POST (10 s, no redirects or proxy env); failures logged. Config: [hub-reference](reference.md#notifytoml); test: palette "Send a test push".

## Doctor

`fw hub doctor`: `ok | info | warn | FAIL` per finding, each with a `fix:`; read-only (binds the port a moment, runs only `--version`/`--help`).

| Check | Fails / warns when |
|---|---|
| `fw` / `flywheel` on PATH | the first match is a script (a Python venv's `fw`: FAIL) or another flywheel build (warn) |
| `hub` | the running hub's version differs (`fw hub upgrade`) |
| `claude` / `codex`, `claude hooks` | missing, no `--version`, or `claude --settings` rejects the hook JSON |
| state dir, secrets | not yours / not 0700, or a secret group/other readable |
| `hub.toml`, `notify.toml`, `settings.json`, plugins | as `fw hub config check` |
| `recordings` | over 90% (info) or past `dir_cap` (warn) |
| `port`, `tailscale`, `browser`, `systemd`, `clock`, `disk` | port taken, no tailnet serve, no opener, no linger / WSL systemd, unsynced clock, < 2 GiB free (warn), < 512 MiB (FAIL) |

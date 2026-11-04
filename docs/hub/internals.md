# Hub internals

How the hub works underneath. Front door: [hub](../hub.md).

## Model

- **Workspace**: unique name + 1..32 **folders** (existing dirs, git or plain), optionally labelled; a **missing** one refuses new sessions.
- **Session**: one PTY in one folder: `shell` (`$SHELL` or an argv), `claude`, or `codex` (on the hub's `PATH`, else 400). `running`, `exited` (code or signal; scrollback stays), or `stopped` (hub stopped under it; `last_cwd`, `restore_error`).
- `state.json`: atomic, schema 5; older migrate, newer refused. Kill hangs up the leader's group and background jobs (Linux: `/proc`).
- One terminal size (max 500x300): a pane resizes the PTY only while focused or sole viewer; others clip. Input past 4 MiB queued for a terminal not reading is dropped (`nack` + `error`).
- Env: [hub-reference](reference.md#environment); enclosing-agent markers (`CLAUDECODE`, `CLAUDE_CODE_*`, `AI_AGENT`, `CODEX_SANDBOX`, ...) are removed.

## Terminal emulation

- One `alacritty_terminal` emulator per session; attach = a **snapshot** (scrollback, screens, cursor, pen, modes, title, palette), then live bytes, no gap or overlap. 2 MiB of scrollback; a client behind gets the screen only (`reset`; protocol 2: a frame).
- Weak links: recent output (kept across `upgrade`) resumes a dropped viewer, row diffs pace slow ones ([hub-api](api.md#terminal-websocket)). A relaunch closes the old sockets (4005).
- Not replayed: custom tab stops, blink/overline. Not emulated: `?47`/`?1047`, kitty keyboard.
- The server answers terminal queries (DA, DSR, DECRQM, OSC 4/10/11/12) once; clients must not. Colours: the last viewer-reported theme (dark first).
- Bounds: OSC strings 64 KiB, titles 4 KiB (stack 10), hyperlinks 2 KiB, 8 combining marks per cell.

## Lifecycle and restore

- `start` re-execs `fw hub serve` detached into `hub.log` (10 MiB, 3 kept); ready = `server.json`, else SIGTERM at `--timeout` with the log tail. `stop` SIGTERMs and waits for the lock; `--force` SIGKILLs (no restore record).
- One hub per state dir (`serve.lock`); a stale `server.json` is removed (the CLI never sends the token to whatever listens there now).
- Shutdown records running sessions `stopped`, then SIGHUP, then SIGKILL (`nohup`ed children too); SIGTERM during startup waits until serving.
- `install-service`: `~/.config/systemd/user/<name>.service` (`KillMode=mixed`, `--no-login-code`) + enable/linger steps.

**Restore** (each start unless `--no-restore`, which drops pending ones for good) of `stopped` sessions: same id, name, order; fresh PTY.

| Recorded as | Comes back as |
|---|---|
| interactive shell (`bash`, `zsh`, ... at most `-l`/`-i`) | same shell in its last cwd (Linux `/proc`), else its folder |
| claude with a transcript | `claude --resume <agent_session_id>`, fresh hooks |
| codex with a rollout for its thread id | `codex -c notify=[...] resume <thread-id>` |
| anything else, or agents under `--no-agent-resume` | a fresh shell (`fallback_command`; never re-runs the command) |
| folder gone | stays `stopped` with `restore_error`, retried each start |
| found `running` (crash) | not restored ("not shut down cleanly"); `fw hub relaunch` |

## Live handoff (`fw hub upgrade`)

Replaces the hub process **without ending any session**: same pids, screens, scrollback, port, status, hook secrets. Linux only.

- Binary: `--binary`, else `fw` on `PATH`, else this one; it inherits the old hub's environment. Earlier agents' hooks keep the old path: upgrade in place or keep it.
- Over `handoff.sock` (same user) the old hub freezes (503, PTYs paused), closes WebSockets with **1012**, passes listener, locks, PTY masters, pidfds (`SCM_RIGHTS`), snapshots, held input, taken input ids, the resume ring, and exits once the new hub acks. Input refused meanwhile is `nack`ed for a resend: never lost or doubled.
- Failure before commit thaws the old hub; a stuck new one is killed at `--timeout`. Refused (exit 4, nothing changed): protocol skew, a pre-handoff hub, a binary without `--handoff`; `--allow-restart` restarts instead (sessions restored, not kept); systemd: `systemctl --user restart <unit>`.
- Handed-over processes are not children: exits via pidfd, code unknown; no restore pass. `attach` / `wait` reconnect within 2 min.

## Agent status

| Status | Meaning | Claude Code | Codex |
|---|---|---|---|
| `working` | busy | `UserPromptSubmit`, `PreToolUse`, `PostToolUse[Failure]` | screen: `Working (12s • esc to interrupt)` |
| `blocked` | waits on a person | `PermissionRequest`; `Notification` permission / elicitation / `agent_needs_input`; screen: folder trust, a known dialog 3 s unhooked | screen: approvals, `request_user_input`, folder trust |
| `done` | finished a turn **this client** has not viewed | `Stop`, `StopFailure` | `notify` `agent-turn-complete` |
| `idle` | seen, or waiting for a first prompt | `SessionStart`; viewed `done`; an interrupted turn | viewed `done`; screen: the composer (`›`) |
| `unknown` | no signal, or agent ended | `SessionEnd`, exit | exit; status line frozen 5 s; no rule matches 3 s |

- **Idle is not authorization**: no running turn, no known prompt on screen; an unknown prompt reads idle or unknown, never blocked.
- Hook / `notify` reports win; screen rules fill gaps; shells have none. Esc fires no Claude hook: a seen turn showing the prompt 3 s reads `idle`, a dialog closing onto a running turn `working`.
- Seen is per client id (REST: once anyone viewed it). A view counts while the page is visible, focused, and used in 5 min; a pane's turn watched from its start counts to 30 min (per session, in splits too).
- Rollup: a workspace's most urgent session, blocked > working > done > idle. Fan-out candidates' finished turns roll up `idle` until a pick; an unseen grading (`needs_you`) is one `done` until the board is opened (`POST .../seen`).
- Claude: `--session-id <uuid>` (`agent_session_id`, follows `/clear`), hooks via `--settings` (merged with yours) running `<this binary> hub report`. Codex: `-c notify=[<this binary> hub report --source codex --event notify]`; `thread-id` is `agent_session_id`. Trust: [hub-security](security.md#trust-model).

## Detection manifests

Per-kind TOML rules read against the screen's **live bottom**: `crates/flywheel-hub/detect/{claude,codex,shell}.toml`, each replaced wholesale by `<state-dir>/agent-detection/<kind>.toml` (re-read within 1 s; invalid: logged, ignored).

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

- Pin `blocked` rules to a dialog's footer so quoted text does not match. Limits: 64 rules, 8 clauses, 16 patterns per list, 400 chars, 64 KiB per file; read at most every 250 ms. Sources: Claude Code 2.1.306 screens (`detect/fixtures/`), `codex-rs/tui` strings; shell rules only answer `explain`.
- `fw hub explain S` (UI `why?`): status, source (`hook`, `manifest` + rule, `heuristic`, `hub`), a fresh screen read with evidence, why each rule missed.

## Terminal client: `fw hub attach`

Any terminal, local or over SSH (`--url`, `FW_HUB_TOKEN`).

- No SESSION: a picker (`j`/`k`, Enter, `/`, `q`). Attached: snapshot, then raw bytes; typing verbatim; the PTY follows this terminal; a relaunch elsewhere reattaches.
- Prefix `C-b` (`--prefix C-a`): `d` detach, `s` switch, `t` status bar (`workspace/session [status]`, `N blocked  N done`; `--no-status`), `r` redraw, `?` keys, twice sends it.
- 256 KiB in flight at most (then a fresh snapshot); the terminal is restored on detach, error, signal, or panic. Token rotated: a state-dir token is re-read, a given one exits 1.

## Layout: tabs and split panes

Per workspace: tabs of split trees, one session per pane, stored on the hub; active tab, focus, zoom per browser. Keys: [hub-reference](reference.md#keyboard-shortcuts).

- Tab bar: `+`, double-click renames, middle-click closes. A sidebar session dropped on a pane edge splits, mid-pane shows it.
- A new-shell split (`Alt \`, `Alt -`) opens an agent's companion shell first; ``Alt ` `` opens it beside its agent above 1100 px.
- Panes stay 40x8 cells+ (else a new tab). Closing a pane (`Alt X`) leaves its session running; removing a session closes its panes and empty tabs. Only the active tab attaches; 12 tabs, 12 panes each. Phones (< 760 px): one pane, a chip row switches.
- Browser checks (opt-in; temp hub, fake `claude`; env `FW`, `PLAYWRIGHT`, `CHROMIUM`, `PORT`): `node crates/flywheel-hub/e2e/layout.mjs [OUT_DIR]` (1440x900, 390x844); `e2e/network.mjs [--baseline] [--full]` (400/150 kbps, 300 ms).


## Web UI

- Sidebar: machines > workspaces (rollup) > sessions (status dot, `↑ahead ↓behind ~changed ?untracked`, companions under their agent, a blocked row's question). Folder chips: filled git, hollow plain, struck missing.
- Title and favicon count blocked and done; `#session=<id>` survives reloads. A dropped or silent (no pong in 6 s) socket keeps the screen and reconnects (2 s, up to 10 s) with what it missed.
- Keys typed while down wait (`N queued`) and go in order once back; dropped, and said, if the session changed (epoch). No local echo; unacked 500 ms: `sending…`. Assets: gzipped, `?v=`-addressed, cached for good.
- Drawers (Commands, Flywheel, Review, Queue) share the stage's right edge; phones: full-screen sheets. Ended sessions: Relaunch, Close; removed: the pane goes (toast).
- First run: import offers, a 30 s tour. Themes: auto, dark, light (dim TUI colours lifted to 4.5:1).
- Embedded (licenses in `web/vendor/`): xterm.js 6.0.0 (+ fit, web-links, search); OFL fonts: JetBrains Mono 2.304, then JuliaMono for its gaps (`tests/glyphs.rs`).

## Phone

Tested at 390x844, 430x932. Pinned to the visual viewport (the keyboard shrinks the terminal); the PTY keeps 16+ rows.

- **Needs you**: blocked and done agents of every workspace (blocked, longest waiting, first) head the sidebar and start screen; a workspace lists them as one row. Footer utilities: one **More** row.
- **Key bar** (touch): `esc` arrows `enter` `^c` (44px, fits 360px); then `tab` `shift-tab` sticky `ctrl` `alt`, `/ | ~ -`, `pgup` `pgdn`, text size, `kbd`, `compose`.
- **Quick answers**: a blocked dialog's options as buttons; AskUserQuestion's labels and descriptions from its input, only when its question's text is on screen. **Compose**: multi-line text as one bracketed paste + Enter; history per device, drafts per session.
- Touch: swipe scrolls, long-press selects, pinch zooms. **Alerts** (per device): blocked or done while hidden or idle 5 min (https or localhost).
- **Lite mode** (Settings > Terminal, `lite` pill; `auto`: the browser says slow, under 64 KB/s, or 3+ round trips at 300 ms+): paced terminals (protocol 2 `lite`), hidden panes none, side panels wait (60 s), re-lists every 3 s at most.
- **Install**: PWA; the service worker skips `/api` and sockets, serving the cached page only when the hub is down.

## Search

`Ctrl Shift F`, `fw hub search`, `GET /api/search`: one query across every session.

- Sources: `scrollback` (soft wraps joined, exited sessions too), `transcript` (current conversation: text, tool calls, results labelled with their call; no thinking), `commands`.
- Literal or `regex` (linear-time, 1 MiB compiled), case-insensitive unless `case`, at most 1 KiB, never across lines. Newest first; 20 per session and source, 200 total; `offset` pages.
- Bounds: 1.5 s (then `partial`), 2 at once, transcript indexes 256 MiB (dropped after 10 min unused); 20 sessions x 5k lines: ~30 ms.
- The UI also asks every reachable machine (6 s each), naming offline ones; CLI `--machine`.

## Recording

`fw hub record start|stop SESSION` (or the `rec` pill); `fw hub record get NAME -o demo.cast; asciinema play demo.cast`.

- Opt-in: `recordings/<session>-<UTC time>.cast`, asciinema v2 (`o`, `r` resizes, `m` markers like `exit: N`), from a screen snapshot. `recordings.part_cap` (64 MiB) rotates to `<base>.part<N>.cast`; `recordings.dir_cap` (1 GiB) deletes the oldest finished.
- Ends on stop, exit, relaunch, or hub stop; `upgrade` continues the file. Peers 403; an active one cannot be deleted (409).
- `/replay#<name>`: seek with marker ticks, 0.5-8x, idle gaps cut to 2 s, follows parts.

## Push notifications

ntfy-compatible POST when an agent becomes **blocked** or **done**, so a phone hears it with every browser closed. Config: [hub-reference](reference.md#notifytoml) (prefer it to `--notify-url`: flags show in `ps`); test: palette "Send a test push".

- `<session> is blocked|done`, workspace / kind / branch, the blocked detail, `Priority: high` when blocked, `Click:` the session URL; never terminal content, paths, or tokens.
- Sent once the status held `settle_secs` (3 s), nobody views it, and a done turn is unseen; once per spell, `debounce_secs` per session and status. One POST (10 s, no redirects or proxy env); failures logged.

## Doctor

`fw hub doctor`: `ok | info | warn | FAIL` per finding, each with a `fix:`; read-only (binds the port a moment, runs `--version`/`--help` only).

| Check | Fails / warns when |
|---|---|
| `fw` / `flywheel` on PATH | the first is a script (a Python venv's `fw`: FAIL) or another build (warn) |
| `hub` | the running hub's version differs |
| `claude` / `codex`, `claude hooks` | missing, no `--version`, `--settings` rejects the hooks |
| state dir, secrets | not yours / not 0700, a secret group/other readable |
| `hub.toml`, `notify.toml`, `settings.json`, plugins | as `fw hub config check` |
| `recordings` | 90% (info) / past `dir_cap` (warn) |
| `port`, `tailscale`, `browser`, `systemd`, `clock`, `disk` | taken, no tailnet serve, no opener, no linger / WSL systemd, unsynced, < 2 GiB free (warn), < 512 MiB (FAIL) |

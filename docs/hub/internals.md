# Hub internals

How the hub works underneath. Front door: [hub](../hub.md).

## Model

- **Workspace**: unique name + 1..32 **folders** (existing dirs, git or plain), optionally labelled; a **missing** one refuses new sessions.
- **Session**: one PTY in one folder: `shell` (`$SHELL` or an argv), `claude`, or `codex` (on the hub's `PATH`, else 400). `running`, `exited` (code or signal; scrollback stays), or `stopped` (hub stopped under it; `last_cwd`, `restore_error`).
- `state.json`: atomic, schema 5; older migrate, newer refused. Kill hangs up the leader's group and background jobs (Linux: `/proc`).
- One terminal size (max 500x300): a pane resizes the PTY only while focused or sole viewer; others clip. Input past 4 MiB queued for a terminal not reading is dropped (`nack` + `error`).
- Env: [hub-reference](reference.md#environment), minus enclosing-agent markers (`CLAUDECODE`, `CLAUDE_CODE_*`, `AI_AGENT`, `CODEX_SANDBOX`, ...).

## Terminal emulation

- One `alacritty_terminal` emulator per session; attach = a **snapshot** (scrollback, screens, cursor, pen, modes, title, palette), then live bytes, no gap or overlap. 2 MiB of scrollback; a client behind gets the screen only.
- Weak links: recent output (kept across `upgrade`) resumes a dropped viewer, row diffs pace slow ones ([hub-api](api.md#terminal-websocket)).
- Not replayed: custom tab stops, blink/overline. Not emulated: `?47`/`?1047`, kitty keyboard.
- The server answers terminal queries (DA, DSR, DECRQM, OSC 4/10/11/12) once; clients must not. Colours: the last viewer-reported theme (dark first).
- Bounds: OSC strings 64 KiB, titles 4 KiB (stack 10), hyperlinks 2 KiB, 8 combining marks per cell.

## Lifecycle and restore

- `start` runs `fw hub serve` detached into `hub.log` (10 MiB, 3 kept); not ready (`server.json`) by `--timeout`: SIGTERM, log tail. `stop` SIGTERMs, waits for the lock; `--force` SIGKILLs (no restore record).
- One hub per state dir (`serve.lock`); a stale `server.json` is removed (the CLI never sends the token to whatever listens there now).
- Shutdown records running sessions `stopped`, then SIGHUP, SIGKILL (`nohup`ed children too); SIGTERM during startup waits until serving.
- `install-service`: `~/.config/systemd/user/<name>.service` (`KillMode=mixed`) + enable/linger steps.

**Restore** (each start unless `--no-restore`) of `stopped` sessions: same id, name, order; fresh PTY.

| Recorded as | Comes back as |
|---|---|
| interactive shell (at most `-l`/`-i`) | same shell in its last cwd (Linux), else its folder |
| claude with a transcript | `claude --resume <agent_session_id>`, fresh hooks |
| codex with a rollout for its thread id | `codex -c notify=[...] resume <thread-id>` |
| anything else, or agents under `--no-agent-resume` | a fresh shell (never re-runs the command) |
| folder gone | stays `stopped` with `restore_error`, retried each start |
| found `running` (crash) | not restored; `fw hub relaunch` |

## Live handoff (`fw hub upgrade`)

Replaces the hub process **without ending any session**: same pids, screens, scrollback, port, status, hook secrets. Linux only.

- Binary: `--binary`, else `fw` on `PATH`, else this one; it inherits the old hub's environment. Earlier agents' hooks keep the old binary's path: upgrade in place.
- Over `handoff.sock` the old hub freezes (503, PTYs paused), closes WebSockets (**1012**), passes listener, locks, PTY masters, pidfds (`SCM_RIGHTS`), snapshots, held input, input ids, the resume ring, and exits once the new hub acks. Input meanwhile is `nack`ed for a resend: never lost or doubled.
- Failure before commit thaws the old hub; a stuck new one is killed at `--timeout`. Refused (exit 4, nothing changed): protocol skew, a pre-handoff hub, a binary without `--handoff`; `--allow-restart` restarts instead (sessions restored, not kept); systemd: `systemctl --user restart <unit>`.
- Handed-over processes are not children: exits via pidfd, code unknown. `attach`, `wait` reconnect within 2 min.

## Agent status

| Status | Meaning | Claude Code | Codex |
|---|---|---|---|
| `working` | busy | `UserPromptSubmit`, `PreToolUse`, `PostToolUse[Failure]` | screen: `Working (12s • esc to interrupt)` |
| `blocked` | waits on a person | `PermissionRequest`; `Notification` permission / elicitation / `agent_needs_input`; screen: a known dialog 3 s unhooked | screen: approvals, `request_user_input`, folder trust |
| `done` | finished a turn **this client** has not viewed | `Stop`, `StopFailure` | `notify` `agent-turn-complete` |
| `idle` | seen, or waiting for a first prompt | `SessionStart`; viewed `done`; an interrupted turn | viewed `done`; screen: the composer (`›`) |
| `unknown` | no signal, or agent ended | `SessionEnd`, exit | exit; status line frozen 5 s; no rule matches 3 s |

- **Idle is not authorization**: an unknown prompt reads idle or unknown, not blocked.
- Hook / `notify` reports win; screen rules fill gaps. Esc fires no Claude hook: a seen turn showing the prompt 3 s reads `idle`, a dialog closing onto a running turn `working`.
- Seen is per client id (REST: anyone). A view counts while the page is visible, focused, used in 5 min; a turn watched from its start counts to 30 min.
- Rollup: a workspace's most urgent session, blocked > working > done > idle. Fan-out candidates roll up `idle` until a pick; an unseen grading is one `done`.
- Claude: `--session-id <uuid>` (`agent_session_id`, follows `/clear`), hooks via `--settings` (merged with yours) running `<this binary> hub report`. Codex: `-c notify=[<this binary> hub report --source codex --event notify]`; `thread-id` is `agent_session_id`.

## Detection manifests

Per-kind TOML rules read against the screen's **live bottom**: `crates/flywheel-hub/detect/{claude,codex,shell}.toml`, each replaced wholesale by `<state-dir>/agent-detection/<kind>.toml` (re-read within 1 s; invalid: logged, ignored).

Start from a shipped (commented) file. Fields: `kind` (the file name), `version` (shown by `explain`); `[[rule]]`s, the first whose every clause holds wins: `id`, `state` (`working` | `blocked` | `idle` | `hold`: keep the status), `detail`, `detail_line` (regexes; capture 1 becomes the detail);
`[[rule.when]]` clauses: `lines` (the last N non-empty rows, 1-100, 30), `line` / `not_line` (regexes), `any` / `all` / `none` (case-insensitive text).

- Pin `blocked` rules to a dialog's footer so quoted text does not match. Limits: 64 rules, 8 clauses, 16 patterns per list, 400 chars, 64 KiB per file; read at most every 250 ms. Shell rules only answer `explain`.
- `fw hub explain S` (UI `why?`): status, source (`hook`, `manifest` + rule, `heuristic`, `hub`), a fresh screen read with evidence, why each rule missed.

## Terminal client: `fw hub attach`

Any terminal, local or over SSH (`--url`, `FW_HUB_TOKEN`).

- No SESSION: a picker. Attached: snapshot, then raw bytes; typing verbatim; the PTY follows this terminal; a relaunch elsewhere reattaches.
- Prefix `C-b` (`--prefix C-a`): `d` detach, `s` switch, `t` status bar (`--no-status`), `r` redraw, `?` keys, twice sends it.
- 256 KiB in flight at most (then a fresh snapshot); the terminal is restored on any exit. Token rotated: a state-dir token is re-read, a given one exits 1.

## Layout: tabs and split panes

Per workspace: tabs of split trees, one session per pane, stored on the hub; active tab, focus, zoom per browser. Keys: [hub-reference](reference.md#keyboard-shortcuts).

- Tab bar: `+`, double-click renames, middle-click closes. A sidebar session dropped on a pane edge splits, mid-pane shows it. A new-shell split opens an agent's companion first.
- Panes stay 40x8 cells+ (else a new tab). Closing a pane leaves its session running; removing one closes its panes, empty tabs. Only the active tab attaches; 12 tabs, 12 panes each. Phones (< 760 px): one pane, a chip row switches.
- Browser checks (opt-in; env `FW`, `PLAYWRIGHT`, `CHROMIUM`): `node crates/flywheel-hub/e2e/{layout,network}.mjs` (network: 400/150 kbps, 300 ms).

## Web UI

- Sidebar: machines > workspaces (rollup) > sessions (status dot, `↑ahead ↓behind ~changed ?untracked`, companions, a blocked row's question). Folder chips: filled git, hollow plain, struck missing.
- Title and favicon count blocked and done; `#session=<id>` survives reloads. A dropped or silent (no pong in 6 s) socket keeps the screen, reconnects (2 s, up to 10 s) with what it missed.
- Keys typed while down wait (`N queued`) and go in order once back; dropped, and said, if the session changed (epoch). Unacked 500 ms: `sending…`. Assets: gzipped, `?v=`-addressed, cached for good.
- Drawers (Commands, Flywheel, Review, Queue) share the stage's right edge; phones: full-screen sheets. Ended sessions: Relaunch, Close. First run: import offers, a 30 s tour. Themes: auto, dark, light (dim TUI colours lifted to 4.5:1).
- Vendored (`web/vendor/`, with licenses): xterm.js 6.0.0 (+ fit, web-links, search); JetBrains Mono 2.304, then JuliaMono for its gaps.

## Phone

Pinned to the visual viewport (the keyboard shrinks the terminal); the PTY keeps 16+ rows.

- **Needs you**: blocked and done agents of every workspace (blocked, longest waiting, first) head the sidebar and start screen. Footer utilities: one **More** row.
- **Key bar** (touch): `esc` arrows `enter` `^c`; then `tab` `shift-tab` sticky `ctrl` `alt`, `/ | ~ -`, `pgup` `pgdn`, text size, `kbd`, `compose`.
- **Quick answers**: a blocked dialog's options as buttons (AskUserQuestion's from its input, only while its question is on screen). **Compose**: multi-line text as one bracketed paste + Enter; history per device, drafts per session.
- Swipe scrolls, long-press selects, pinch zooms. **Alerts** (per device, https or localhost): blocked or done while hidden or idle 5 min.
- **Lite mode** (Settings > Terminal; `auto`: the browser says slow, under 64 KB/s, or 3+ round trips at 300 ms+): paced terminals (protocol 2 `lite`), hidden panes none, side panels wait (60 s), re-lists every 3 s at most.
- **Install**: PWA; the service worker serves the cached page only when the hub is down.

## Search

`Ctrl Shift F`, `fw hub search`, `GET /api/search`: one query across every session.

- Sources: `scrollback` (exited sessions too), `transcript` (text, tool calls, results; no thinking), `commands`.
- Literal or `regex` (linear-time), case-insensitive unless `case`, at most 1 KiB, one line. Newest first; 20 per session and source, 200 total; `offset` pages.
- Bounds: 1.5 s (then `partial`), 2 at once, transcript indexes 256 MiB (dropped after 10 min unused); 20 sessions x 5k lines: ~30 ms.
- The UI also asks every reachable machine (6 s each), naming offline ones; CLI `--machine`.

## Recording

`fw hub record start|stop SESSION` (or the `rec` pill); `fw hub record get NAME -o demo.cast; asciinema play demo.cast`.

- Opt-in: `recordings/<session>-<UTC time>.cast`, asciinema v2 (`o`, `r` resizes, `m` markers: `exit: N`, `resync` after falling behind, `continued in`), from a screen snapshot. `recordings.part_cap` rotates to `<base>.part<N>.cast`, opening on the screen the last part ended on: no output is skipped; `recordings.dir_cap` deletes the oldest finished.
- Ends on stop, exit, relaunch, hub stop; `upgrade` continues the file. Peers 403; an active one is not deleted (409).
- `/replay#<name>`: seek with marker ticks, 0.5-8x, idle gaps cut to 2 s, follows parts.

## Push notifications

ntfy-compatible POST when an agent becomes **blocked** or **done**, so a phone hears it with every browser closed. Config: [hub-reference](reference.md#notifytoml); test: palette "Send a test push".

- `<session> is blocked|done`, workspace, kind, branch, the blocked detail, `Click:` the session URL (`Priority: high` if blocked); never terminal content, paths, tokens.
- Sent once the status held `settle_secs`, nobody views it, and a done turn is unseen; once per spell, `debounce_secs` per session and status. One POST (10 s, no redirects or proxy env); failures logged.

## Doctor

`fw hub doctor`: `ok | info | warn | FAIL` per finding, each with a `fix:`; read-only (binds the port a moment, runs `--version`/`--help` only).

Checks: `fw` / `flywheel` on PATH (a script or other program: FAIL; another build: warn), the running `hub`'s version, `claude` / `codex` and `claude hooks` (missing, no `--version`, `--settings` rejects the hooks),
state dir and secrets (owner, 0700 / 0600), the config files (as `fw hub config check`), `recordings` (90%: info, past `dir_cap`: warn), `port`, `tailscale`, `browser`, `systemd` (linger, WSL), `clock`, `disk` (< 2 GiB warn, < 512 MiB FAIL).

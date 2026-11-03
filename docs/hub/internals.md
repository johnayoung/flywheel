# Hub internals

How the hub behaves underneath the UI. Front door: [hub](../hub.md).

## Model

- **Workspace**: a unique name + 1..32 **folders** (existing dirs, absolute or `~/`), each a git work-tree root or a plain dir, with an optional label. Repo-only features (worktrees, branch badges) apply where `git` is true.
- A **missing** folder (moved, unmounted) refuses new sessions until it is back or removed. `git` / `missing` are re-read on every listing.
- **Session**: one PTY in one folder. Kind `shell` (`$SHELL` or an argv), `claude`, or `codex` (resolved on the hub's `PATH`; missing CLI = 400).
- Session states: `running`, `exited` (code or signal; its PTY closes, scrollback stays attachable), `stopped` (hub stopped while it ran; carries `last_cwd`, `restore_error`).
- Persisted in `<state-dir>/state.json` (atomic writes, schema 5 -- 2 layouts, 3 launch templates, 4 fan-outs, 5 prompt queues; older files migrate, newer are refused).
- Kill hangs up the whole session: the leader's group plus background jobs (Linux scans `/proc`; elsewhere the leader's group only).
- Terminal size is shared (max 500x300). In the web UI a pane resizes the PTY only while focused or sole viewer, after a divider drag ends; other panes clip to the shared size (badge `NNxNN · another viewer's size`).
- Input queued past 4 MiB for a terminal that is not reading is dropped (client gets `error`).
- Session env: `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`; agents also get the report socket and secret. Enclosing-agent markers (`CLAUDECODE`, `CLAUDE_CODE_*`, `AI_AGENT`, `CODEX_SANDBOX`, ...) are removed so agents never think they are nested.

## Terminal emulation

- One `alacritty_terminal` emulator per session on the server. Attach = a **snapshot** (scrollback, screen, alt screen, cursor, pen, modes, title, palette) then live bytes, no gap or overlap.
- Snapshot holds the newest 2 MiB of scrollback; a client that falls behind gets a screen-only snapshot (`reset`).
- Not replayed: custom tab stops, blink/overline. Not emulated: `?47`/`?1047` alt screens, kitty keyboard protocol.
- The server answers terminal queries (DA, DSR, DECRQM, OSC 4/10/11/12) once per session; clients must not. Colour answers use the last viewer-reported theme (dark before any).
- Memory bounds: OSC strings 64 KiB, titles 4 KiB (stack 10), hyperlinks over 2 KiB dropped, 8 combining marks per cell.

## Lifecycle and restore

- `start` re-execs `fw hub serve` detached (`setsid`, cwd `/`), logging to `hub.log` (rotated at 10 MiB, 3 kept). Ready = `server.json` written. Not ready within `--timeout` (30 s): SIGTERMed, log tail printed.
- `stop` sends SIGTERM and waits for the serve lock; `--force` SIGKILLs after `--timeout` (no restore record).
- One hub per state dir: `serve.lock` is held for the hub's life. A stale `server.json` (lock free) is ignored and removed, so the CLI never sends the token to whatever listens there now.
- Shutdown (Ctrl-C / SIGTERM) records running sessions `stopped`, then SIGHUP, then SIGKILL to anything left (`nohup`ed children too). SIGTERM during startup waits until serving.
- `serve` prints one-time login URLs; with a remote-reachable URL and a terminal on stdout it also draws a QR.

**Restore** (each start unless `--no-restore`) of sessions a clean shutdown recorded `stopped`: same id, name, order; fresh PTY.

| Recorded as | Comes back as |
|---|---|
| interactive shell (`bash`, `zsh`, ... at most `-l`/`-i`) | same shell, in its last cwd (Linux `/proc`), else its folder |
| claude with a transcript | `claude --resume <agent_session_id>` with fresh hooks |
| codex with a rollout for its thread id | `codex -c notify=[...] resume <thread-id>` |
| anything else, or any agent under `--no-agent-resume` | a fresh shell (`fallback_command` set; never re-runs the command) |
| folder gone | stays `stopped` with `restore_error`, retried each start |
| found `running` (crash) | not restored: `restore_error` "not shut down cleanly"; `fw hub relaunch` |

`--no-restore` drops pending restores for good. Creating or relaunching in a missing dir is a 400, never a shell in `$HOME`. Per-kind decision: `restore::plan`.

`install-service` writes `~/.config/systemd/user/<name>.service` (`KillMode=mixed` so sessions are recorded for restore, `--no-login-code` so no code lands in the journal) and prints the enable/linger steps; it enables nothing.

## Live handoff (`fw hub upgrade`)

Replaces the hub process **without ending any session**: same pids, screens, scrollback, port, agent status, hook secrets. Use after every rebuild. Linux only (macOS compiles, untested).

- Binary: `--binary`, else `fw` on `PATH`, else this program; probed with `hub start --help` first. The new hub inherits the old hub's environment (`/proc/<pid>/environ`).
- Agents spawned earlier keep hooks pointing at the old binary path: upgrade in place (`cargo install`) or keep it until they relaunch.
- Mechanism: over `handoff.sock` (same-user only) the old hub freezes (listener stays open, open requests get 503, PTYs paused), closes WebSockets with **1012**, and passes listener, locks, PTY masters, pidfds, emulator snapshots, and held input via `SCM_RIGHTS`. It exits only after the new hub acks.
- Input reaching the old hub after handover is refused (503), never lost or typed twice.
- A prompt-queue delivery under way finishes first; none starts until the new hub took over. Typing times and unsent drafts carry over.
- Failure before commit thaws the old hub (it keeps serving); a stuck new hub is killed after `--timeout`.
- Refused (exit 4, nothing changed): protocol skew, a pre-handoff hub, a binary without `--handoff`; for these `--allow-restart` falls back to stop + start with the same flags and env (sessions restored, not kept). Another handoff in progress: retry. A systemd unit's hub: `systemctl --user restart <unit>`.
- Handed-over processes are no longer children: exits are seen via pidfd with **unknown exit code** (`exited ?`). The new hub runs no restore pass.
- Clients reconnect on 1012: browsers at once ("Hub upgraded" toast), `attach` and `wait` within 2 min.

## Agent status

| Status | Meaning | Claude Code | Codex |
|---|---|---|---|
| `working` | busy | `UserPromptSubmit`, `PreToolUse`, `PostToolUse[Failure]` | screen: status line (`Working (12s • esc to interrupt)`) |
| `blocked` | waits on a person | `PermissionRequest`; `Notification` permission / elicitation / `agent_needs_input`; screen: folder-trust or a known dialog 3 s with no hook | screen: approvals, `request_user_input`, folder trust |
| `done` | finished a turn **this client** has not viewed | `Stop`, `StopFailure` | `notify` `agent-turn-complete` |
| `idle` | seen, or waiting for a first prompt | `SessionStart`; viewed `done`; an interrupted turn | viewed `done`; screen: the composer (`›`) |
| `unknown` | no signal, or agent ended | `SessionEnd`, exit | exit; status line frozen 5 s; no rule matches 3 s |

- **Idle is not authorization**: it means no running turn and no known prompt on screen. An unknown prompt reads idle or unknown, never blocked.
- Precedence: hook / `notify` reports win; screen rules fill only what reports miss. Shells have no status.
- Interrupts (Esc) fire no Claude hook: a turn that was seen on screen and then shows the prompt for 3 s reads `idle`. A `blocked` dialog closing onto a running turn reads `working`.
- Seen is per client id (browser local storage, or the attach client). The REST API counts a turn seen once any client viewed it.
- Rollup: a workspace shows its most urgent session, blocked > working > done > idle.
- Claude: hub assigns `--session-id <uuid>` (`agent_session_id`, follows `/clear`) and injects hooks via `--settings` (merged with yours) that run `<this binary> hub report`.
- Codex: launched with `-c notify=["<this binary>","hub","report","--source","codex","--event","notify"]`; its `thread-id` becomes `agent_session_id`.
- Report channel and trust: [hub-security](security.md#trust-model).

## Detection manifests

Per-kind TOML screen rules read against the **live bottom** of the server's screen. Bundled: `crates/flywheel-hub/detect/{claude,codex,shell}.toml`. Override: `<state-dir>/agent-detection/<kind>.toml` replaces the bundled file wholesale, re-read within 1 s; an invalid override is logged and the bundled file stays.

```toml
kind = "codex"            # must match the file name
version = 2               # shown by explain
[[rule]]                  # first rule whose every clause holds wins
id = "approval"
state = "blocked"         # working | blocked | idle | hold (keep the status)
detail = "asks for approval"
detail_line = ['(?i)^\s*(would you like to .+\?)$']   # capture 1 becomes the detail (wins over detail)
[[rule.when]]
lines = 1                 # last N non-empty rows (1-100, default 30)
line = ['(?i)^\s*press enter to confirm or esc to cancel\s*$']
[[rule.when]]
lines = 100
any = ["would you like to run the following command?"]   # also all, none, not_line
```

- Pin `blocked` rules to a dialog's footer on the last row (as the bundled ones do) so quoted dialog text does not match.
- Limits: 64 rules, 8 clauses each, 16 patterns per list, 400 chars per pattern, 64 KiB per file; linear-time `regex`. Read at most every 250 ms after output.
- Claude rules come from Claude Code 2.1.306 screens (`detect/fixtures/`); Codex rules from `codex-rs/tui` strings. Shell rules only answer `explain`.
- `fw hub explain S` (UI: `why?` beside the status pill): the status, its source (`hook`, `manifest` + file + rule, `heuristic`, `hub`), a fresh screen read with evidence rows, and why each rule did not match.

## Terminal client: `fw hub attach`

Any terminal, local or over SSH; a remote hub with `--url` + `FW_HUB_TOKEN`.

- No SESSION: a picker (`j`/`k`, Enter, `/` filter, `q`). Stopped sessions are listed but need `fw hub relaunch`.
- Attached: snapshot then raw bytes; typing goes verbatim. The PTY follows this terminal's size. Counts as viewing.
- Prefix `C-b` (`--prefix C-a`): `d` detach, `s` switch, `t` status bar, `r` redraw, `?` keys, prefix twice sends it.
- Status bar (last row): `workspace/session [status]` and hub-wide `N blocked  N done`; `--no-status` hides it.
- Flow control: at most 256 KiB in flight; a slow terminal gets a fresh snapshot instead of a queue.
- The hub answers terminal queries; they are not passed to your terminal. The terminal is restored on detach, error, signal, or panic.
- Token rotation: a state-dir token is re-read and reconnects; a hand-given token exits 1.

## Layout: tabs and split panes

Per workspace: named tabs, each a split tree whose panes show one session. Stored on the hub (shared by every browser); active tab, focus, and zoom are per browser.

- Desktop: tab bar (`+`, double-click renames, middle-click closes); each pane has its own socket and title bar. Drag a sidebar session onto a pane edge to split, its middle to show it there.
- Splitting with a new shell (`Alt \`, `Alt -`) opens an agent's companion shell first, else a shell in the session's folder.
- ``Alt ` `` opens the companion beside its agent on screens over 1100 px.
- Panes never go under 40x8 cells; without room the session opens in a new tab.
- Closing a pane (`Alt X`) only leaves the layout; the session runs on. Removing a session closes its panes and any tab left empty.
- Only the active tab's panes are attached. Max 12 tabs per workspace, 12 panes per tab.
- Phones (< 760 px): no splits; one pane, a chip row switches sessions; phones never change layouts.
- Browser check (opt-in, not in `scripts/check.sh`): `node crates/flywheel-hub/e2e/layout.mjs [OUT_DIR]` drives layouts, phone landing, header pills, and tap targets at 1440x900 and 390x844 against a temp hub with a fake `claude` (env `FW`, `PLAYWRIGHT`, `CHROMIUM`, `PORT`).

Keys: [hub-reference](reference.md#keyboard-shortcuts).

## Web UI

- Sidebar: machines > workspaces (rollup chip) > sessions (kind glyph, status dot, branch line `↑ahead ↓behind ~changed ?untracked`). Companion shells hang under their agent; a blocked row shows its question.
- Folder chips: filled dot = git, hollow = plain, struck red = missing.
- Tab title and favicon count blocked and done agents. Updates are pushed over the events socket; the UI polls only while it is down.
- `#session=<id>` in the URL survives reloads. A dropped socket keeps the last screen and reconnects with backoff (at once on `online` / focus).
- An ended session offers Relaunch and Close. A session removed elsewhere leaves its pane with a toast.
- New workspace: paste folder paths (checked live) or **Import VS Code workspace** (files under the import root, Import / Update with a preview).
- New session: kind, folder (defaults to the attached session's), optional new sibling worktree with live path / branch / base preview.
- First run (no workspace): import offers, a folder path field, and a 30-second shortcut tour (palette "Shortcut tour" replays it).
- Themes: auto, dark, light (light lifts low-contrast TUI colours to 4.5:1).
- Served from the binary: xterm.js 6.0.0 + fit, web-links, search addons and the UI fonts (SIL OFL), vendored with licenses under `crates/flywheel-hub/web/vendor/`. Public without a token: fonts, icons, `login.js`, manifest, `sw.js`.

## Phone

Tested at 390x844 and 430x932 (iPhone 14 emulation).

- The app is pinned to the visual viewport: the keyboard shrinks the terminal. Phones never size the PTY under 16 rows (shorter boxes clip around the cursor).
- **Needs you**: blocked and done agents of every workspace, blocked and longest-waiting first, head the sidebar and are the phone's start screen.
- **Key bar** (touch screens): row 1 `esc` sticky `ctrl` arrows `enter` `^c` (44px keys); row 2 `tab` `shift-tab` sticky `alt`, `/ | ~ -`, `pgup` `pgdn`, text size, `kbd`, `compose`. Modifiers apply to key-bar keys too.
- **Quick answers**: while blocked, the live dialog's options become buttons (numbered lists send the digit; `(y/n)` sends the letter + Enter).
- **Compose**: multi-line text (dictation) sent as one bracketed paste + Enter; per-device history, per-session drafts; sign-out clears both.
- Touch: swipe scrolls with momentum, long-press selects, pinch sets the text size.
- **Alerts** (per device): a system notification when an agent becomes blocked or done while the page is hidden or idle 5 min. Needs https or localhost; on iOS only from the Home Screen app.
- **Install**: PWA manifest + service worker. The worker never touches `/api` or sockets; it serves the cached page only when the hub is unreachable. iOS Home Screen apps need their own sign-in (`fw hub url --qr`).

## Search

`Ctrl Shift F`, `fw hub search`, or `GET /api/search`: one query across every session.

- Sources: `scrollback` (emulator text, soft wraps joined, exited sessions included), `transcript` (each agent's current conversation: user/assistant text and tool calls; no thinking or tool output), `commands` (saved suggested blocks).
- Query: literal or `regex` (linear-time Rust `regex`, 1 MiB compiled cap), case-insensitive unless `case`, at most 1 KiB. A match never spans lines.
- Results grouped by session, newest first, one hit per line / message / block with a one-line snippet. Defaults: 20 per session and source, 200 total; `offset` pages.
- Bounds: 1.5 s per request (then `partial`), 2 searches at once, transcript indexes capped at 256 MiB total (dropped after 10 min unsearched).
- Measured (release, 20 sessions x 5k lines): ~30 ms after new output, 0-6 ms otherwise (`cargo test -p flywheel-hub --release --test search twenty -- --nocapture`).
- With machines registered, the web UI asks every reachable machine in parallel (6 s each); offline ones are named, not searched. CLI: `--machine NAME`.
- Opening a hit: terminal hits scroll to and select the match; transcript hits open the viewer at that message; command hits open the Commands drawer.

## Recording

```bash
fw hub record start agent        # or the header's rec pill
fw hub record stop agent; fw hub record ls
fw hub record get <name> -o demo.cast; asciinema play demo.cast   # or /replay#<name>
```

- Opt-in per session: `<state-dir>/recordings/<session>-<UTC time>.cast`, asciinema v2 (`o` output, `r` resizes, `m` markers such as `exit: N`). Starts with a screen snapshot.
- Caps (live): `recordings.part_cap` (64 MiB) rotates to `<base>.part<N>.cast`; `recordings.dir_cap` (1 GiB) deletes the oldest finished first.
- Ends on stop, process exit, relaunch, or hub stop. `fw hub upgrade` continues the same file without a gap; a normal restart does not resume.
- This hub only: no `--machine`; peer credentials get 403. An active recording cannot be deleted (409).
- Player `/replay#<name>`: play/pause, seek with marker ticks, 0.5-8x, skip idle (gaps over 2 s play as 2 s), follows parts.

## Push notifications

ntfy-compatible POST when an agent becomes **blocked** or **done**, so a phone hears it with every browser closed.

```bash
fw hub start --notify-url https://ntfy.sh/<long-random-topic>   # prefer notify.toml: flags show in ps
```

- Message: `<session> is blocked|done`, workspace / kind / branch, the blocked detail, `Priority: high` for blocked, `Click:` the session URL when the hub has a public or tailnet URL. Never terminal content, paths, or tokens.
- Sent only when the status holds `settle_secs` (3 s), nobody is viewing the session, and a done turn is still unseen. Once per spell; `debounce_secs` per session and status.
- One POST, 10 s timeout, no redirects, no proxy env, rustls with system roots; failures go to the hub log.
- Config: [hub-reference: notify.toml](reference.md#notifytoml). Test: palette "Send a test push" (`POST /api/notify/test`).

## Doctor

`fw hub doctor`: `ok | info | warn | FAIL` per finding, each with a `fix:`. Read-only (the port is bound for a moment; programs get only `--version`/`--help`).

| Check | Fails / warns when |
|---|---|
| `fw` / `flywheel` on PATH | the first match is a script (e.g. a Python venv's `fw`: FAIL) or another flywheel build (warn) |
| `hub` | the running hub's version differs from this CLI (suggests `fw hub upgrade`) |
| `claude` / `codex`, `claude hooks` | missing, no `--version`, or the hook JSON is rejected by `claude --settings` |
| state dir, secrets | not yours / not 0700, or a secret file is group/other readable |
| `hub.toml`, `notify.toml`, `settings.json`, plugins | parse or validation errors (same as `fw hub config check`) |
| `recordings` | over 90% (info) or past `dir_cap` (warn) |
| `port`, `tailscale`, `browser`, `systemd`, `clock`, `disk` | port taken (suggests a free one), no tailnet serve, no opener, no linger / WSL systemd, unsynced clock, < 2 GiB free (warn), < 512 MiB (FAIL) |

# Hub

`fw hub` is a web workspace for AI coding agents. The hub server owns every terminal: each session is a real PTY (shell, prompts, running processes, scrollback) that browsers attach to and detach from without disturbing it. Many clients can watch one session. Crate: `crates/flywheel-hub`.

**Security first:** a hub serves shells over HTTP -- anyone with the token has a shell as you. It binds `127.0.0.1` by default; for remote access put it behind `tailscale serve` rather than `--bind 0.0.0.0`.

## Verbs

```bash
fw hub start [SERVE FLAGS] [--timeout SECS]   # detached; returns once serving, prints login URLs
fw hub stop [--timeout SECS] [--force]        # graceful: sessions recorded for restore
fw hub status [--json]                        # pid, uptime, url, counts; exit 0 running, 3 not, 1 unreachable
fw hub logs [-f] [-n N]                       # <state-dir>/hub.log (detached hubs only)
fw hub install-service [SERVE FLAGS] [--name N] [--force] [--print]   # systemd --user unit; never enabled
fw hub serve [SERVE FLAGS]                    # foreground
fw hub url [--qr]                             # one-time login URLs for the running hub (+ QR for a phone)
fw hub token [--rotate]                       # print the access token (created on first use) / rotate it
fw hub commands SESSION [--last] [--all] [--json] [--copy] [--limit N]   # agent-suggested code blocks
fw hub report --source claude|codex --event E [--detail D] [--stdin] [PAYLOAD]   # agent hooks only
# SERVE FLAGS: [--bind ADDR] [--port N] [--public-url URL]... [--allow-origin ORIGIN]... [--trusted-proxy IP]... [--scrollback-lines N] [--no-restore] [--no-agent-resume]
# every verb takes --state-dir DIR
```

Scripting the running hub (sessions/workspaces by id, unique id prefix, or name; exact name beats prefix; ambiguity is an error):

```bash
fw hub ls [--json]                                         # workspaces > sessions: short id, kind, state (+ agent status), name, live cwd
fw hub new WS [--kind shell|claude|codex] [--repo PATH] [--name N] [--create] [--json] [-- ARGV...]   # prints the id; a taken --name is refused
fw hub send SESSION TEXT|- [--enter] [--raw]               # pasted: newlines -> CR, bracketed when the program enabled it
fw hub read SESSION [--source visible|recent] [--lines N] [--json]
fw hub kill SESSION [--rm]                                 # hang up (record kept, as exited); --rm also forgets it
fw hub relaunch SESSION                                    # start an exited/stopped session again in place
```

`new --create` makes workspace `WS` (with `--repo`) when none matches; relative `--repo` is resolved against the caller's directory. `send` wraps text in `ESC[200~ ... ESC[201~` only when the program set bracketed paste (markers inside the text are stripped, repeatedly, so split markers cannot reassemble), a final newline becomes that Enter (`printf 'make\n' | fw hub send s -` runs it), and sends Enter 50 ms later as its own keypress; `--raw` sends bytes untouched (`fw hub send s $'\x03' --raw` is Ctrl-C).

## Lifecycle and restore

- `start` re-execs `fw hub serve` in a new session (`setsid`, cwd `/`, stdin `/dev/null`), stdout/stderr into `<state-dir>/hub.log` (0600; rotated at 10 MiB, 3 kept as `hub.log.1..3`). Ready = it wrote `server.json` with its pid (after binding and restoring). Failing to start prints the log tail; not ready within `--timeout` (30 s) it is SIGTERMed. Already running: prints it, exits 0. One-time codes and the token never go to the log.
- `stop` sends SIGTERM (asking the hub nothing first) and waits up to `--timeout` for the serve lock to free; no hub running exits 0. `status` waits at most 3 s for the hub to answer. `--force` SIGKILLs after `--timeout` (sessions then not recorded for restore).
- **Restore** (on every start unless `--no-restore`): sessions recorded `stopped` by a clean shutdown come back with the same id, name, and order, a fresh PTY in the directory their process was in at shutdown (Linux `/proc`; else, or when gone, the session's repo). Interactive shells (`sh`, `bash`, `zsh`, `fish`, ... with at most `-l`/`-i`) relaunch as recorded. Agents resume their conversation in their repo, with fresh status hooks and secret: `claude --resume <agent_session_id>` once its transcript exists, `codex -c notify=[...] resume <thread-id>` once a rollout for the reported thread id exists (`$CODEX_HOME` or `~/.codex`, `sessions/**/rollout-*-<id>.jsonl`). Anything else -- a shell running a command, an agent with nothing to resume, any agent under `--no-agent-resume` -- comes back as a fresh hub shell (never re-runs the command; the per-kind decision is `restore::plan`). A fallback keeps the record's `kind`/`command` and sets `fallback_command` (cleared by a relaunch). A session whose repo dir is gone stays `stopped` with `restore_error` (retried each start; UI and `ls` show "not restored"); a gone `last_cwd` restores in the repo with a note in the log. Sessions found `running` after a crash get `restore_error` "not shut down cleanly" and are not restored (`start` lists them; `fw hub relaunch`). `--no-restore` drops pending restores for good (later starts do not restore them). SIGTERM during startup is held until serving, then shuts down cleanly. Creating or relaunching a session whose directory is missing is a 400, never a shell in `$HOME`.
- `install-service` writes `$XDG_CONFIG_HOME/systemd/user/<name>.service` (`~/.config/...`): `ExecStart=<this binary> hub serve ...` (absolute state dir), current `PATH`/`SHELL`, `Restart=on-failure`, `KillMode=mixed` (SIGTERM reaches the hub first, so sessions are recorded for restore). It passes `--no-login-code`: no one-time code is written to the journal; use `fw hub url`. Prints the `systemctl --user enable --now` / `loginctl enable-linger` steps; enables nothing.

`serve` runs in the foreground and prints one-time login URLs (below); when any is reachable from another device (non-loopback bind, `--public-url`, or a detected `tailscale serve`) it also draws a QR code of the first (only when stdout is a terminal; logs get a pointer to `url --qr`). One hub per state dir: the hub holds an exclusive lock on `<state-dir>/serve.lock` for its whole life (guarding `server.json` and `report.sock`), and `serve` refuses to start while another holds it. Ctrl-C / SIGTERM records every running session stopped, then hangs them up (SIGHUP, SIGKILL after a grace). While it runs, `<state-dir>/server.json` (0600) records its pid, address, and public URLs for `url` / `token --rotate`; one left behind by a crashed hub (lock not held) is ignored and removed, so the CLI never sends the token to whatever now listens there.

`url` asks the running hub for a fresh one-time code and lists it under every URL that reaches the hub, most remote-friendly first: `public` (`--public-url`), `tailnet https (tailscale serve)`, `tailnet (MagicDNS)` / `tailnet` (100.x, only when an interface carries it: userspace-networking tailscale cannot reach a bind), `lan (<iface>)` (non-loopback binds; container bridges skipped), `this machine`. `--qr` draws the first remote one (black-on-white half blocks, any terminal theme). The long-lived token is never printed by `serve` or `url`.

`token --rotate` replaces the token. A running hub switches at once (no restart): every browser cookie, outstanding code, and open terminal or events socket (closed with code 4001; the UI returns to sign-in) of the old token stops working; bearer scripts need `fw hub token` again. Agent status hooks are unaffected (they authenticate with per-spawn secrets, not the token). With no hub running it just rewrites the file.

`report` is what agent sessions' hooks run (see Agent status); it needs the session's environment, never prints to stdout, and fails with exit 1 (never 2, which would stop a Claude `Stop`).

| Flag | Default | |
|---|---|---|
| `--bind` | `127.0.0.1` | non-loopback prints a warning |
| `--port` | `7420` | `0` picks a free port |
| `--public-url` | none | origin browsers reach the hub at through a proxy (`https://box.tailnet.ts.net`); accepted as an origin, listed first by `url`, `Secure` cookies when https. Origin only (no path) |
| `--trusted-proxy` | none | proxy on another machine whose `X-Forwarded-*` are believed (loopback always is), so its clients are rate-limited per client, not as one address |
| `--allow-origin` | none | extra browser origin accepted without being listed; same-origin always allowed |
| `--scrollback-lines` | `5000` | per-session scrollback; capped at 1M cells (lines x cols), so wide terminals keep fewer |
| `--state-dir` | `$XDG_STATE_HOME/flywheel/hub`, else `~/.local/state/flywheel/hub` | |

## Remote access (phone, other machines)

Recommended -- Tailscale (HTTPS on the tailnet, which phones need for the clipboard API):

```bash
fw hub serve                                     # loopback only
tailscale serve --bg http://127.0.0.1:7420       # https://<machine>.<tailnet>.ts.net -> the hub
fw hub url --qr                                  # scan with the phone
```

- `serve` and `url` run `tailscale status --json` / `tailscale serve status --json` (3 s timeout; `tailscale` on `PATH` or the macOS app): a `serve` whose `/` proxies to the hub's port is listed and, when found at startup, accepted as an origin. Tailnet up but nothing served: they print the `tailscale serve` command.
- Behind any reverse proxy on this machine, `X-Forwarded-For` (last entry), `X-Forwarded-Host` (counts as same-origin), and `X-Forwarded-Proto` are believed only from loopback peers (IPv4-mapped ones under `--bind ::` included) and `--trusted-proxy` addresses. A proxy elsewhere: pass `--public-url` and `--trusted-proxy`. `tailscale serve` keeps `Host` and sets the forwarded headers, so starting it after `fw hub serve` works (it is just not listed as an origin until the next start).
- LAN: `fw hub serve --bind 0.0.0.0` (warns; plain HTTP, so the clipboard API is off on phones), then `fw hub url --qr`.

## Model

- **Workspace** = a name (unique) + 1..N git work-tree roots (absolute or `~/...`; a `.git` dir or linked-worktree `.git` file).
- **Session** = one PTY process in one of its workspace's repos, of a **kind**: `shell` (the hub's `$SHELL`, or an explicit argv), `claude`, or `codex` (the plain CLI, resolved on the hub's `PATH`; a missing CLI is a 400). States: `running`, `exited` (code or signal), `stopped` (the hub stopped while it ran; `clean` when by its own shutdown, plus `last_cwd` / `restore_error`). An exited session keeps its scrollback attachable until removed or the hub restarts.
- Persisted to `<state-dir>/state.json` (0600, atomic writes). Restarts restore sessions (above).
- Killing a session hangs up its whole session (leader's group plus background jobs, even after the leader exited; Linux scans `/proc`, elsewhere only the leader's group). Removing a session, or deleting its workspace, also frees its PTY.
- Terminal size is shared: the last client to resize sets it for everyone.
- Input beyond 4 MiB queued for a terminal that is not reading is dropped (the client gets an `error` message).
- The server runs a terminal emulator per session (`alacritty_terminal`). Attaching sends a **snapshot** -- escape sequences that rebuild scrollback, screen, alternate screen (with the shell screen under it), cursor, saved cursor, SGR pen, scroll region, charsets, DEC modes (cursor keys, keypad, bracketed paste, mouse, focus), cursor style, title, palette -- then live bytes, with no gap or overlap. The snapshot carries the newest scrollback up to 2 MiB (whole lines); a client that falls behind gets the screen alone. Not replayed: custom tab stops, blink/overline; `?47`/`?1047` alt screens and the kitty keyboard protocol are not emulated.
- The server answers terminal queries (DA, DSR/cursor position, DECRQM, OSC 4/10/11/12 colours) once per session, attached or not; clients must not answer them. Colour answers use the viewer's theme: the web UI reports its palette per session on attach, theme change, and window focus (last report wins; the dark theme before any).
- Output cannot grow server memory without bound: OSC strings are cut at 64 KiB, titles at 4 KiB (title stack 10 deep), hyperlinks over 2 KiB dropped, at most 8 combining marks per cell.
- Sessions get `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`; agent sessions also `FLYWHEEL_HUB_REPORT_SOCKET` and `FLYWHEEL_HUB_REPORT_SECRET`. Markers of an enclosing Claude Code / Codex session (`CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ID`, `AI_AGENT`, `CODEX_SANDBOX`, ...) are removed, so a hub started from inside an agent does not make its agents think they are nested (Claude would stop saving transcripts).

## Agent status

| Status | Meaning | Claude Code | Codex |
|---|---|---|---|
| `working` | busy | `UserPromptSubmit`, `PreToolUse`, `PostToolUse[Failure]` | its busy line ("esc to interrupt") is drawn |
| `blocked` | waits on a person | `PermissionRequest` (detail: the question, for `AskUserQuestion`); `Notification` `permission_prompt` / `elicitation_dialog` / `elicitation_url_dialog` / `agent_needs_input`; `idle_prompt` while working; the folder-trust dialog on screen before any hook | -- (approval prompts are not detected) |
| `done` | finished a turn **this client** has not viewed | `Stop`, `StopFailure` | `notify` `agent-turn-complete` |
| `idle` | finished and seen, or waiting for a first prompt | `SessionStart`; `done` once viewed; an interrupted turn (below) | `done` once viewed |
| `unknown` | no signal yet, agent ended, or Codex's busy line went quiet 5 s with no turn-complete | `SessionEnd`, exit | exit |

- **Interrupts** (Esc) fire no Claude hook. The hub watches the session's screen: once a `working`/`blocked` spell has drawn Claude's `esc to interrupt` footer or a dialog's `Esc to cancel`, and both are then gone for 3 s with no hook, the turn reads as interrupted (`idle`). A permission dialog that closes onto the busy footer was approved: `working` again (its `PostToolUse` only comes when the tool finishes). A spell whose footer never showed (a narrow terminal cuts it) is left alone; viewing the transcript (`ctrl+o`) mid-turn for 3 s reads as idle until the next hook.
- **Seen is per client** (a browser keeps an id in local storage): viewing a session (attached, page visible and focused) turns its `done` into `idle` for that client only; a turn that finishes while a client watches is `idle` for it at once. The REST API counts a turn seen once any client viewed it.
- **Rollup**: a workspace shows its most urgent session: blocked > working > done > idle.
- **Claude**: the hub assigns `--session-id <uuid>` (stored as `agent_session_id`; follows `/clear`) and injects hooks with `--settings '<json>'` (merged with your own settings; `args` form, no shell) that run `<this binary> hub report ...`. Relaunching resumes the conversation (`--resume <id>`) once Claude has a transcript for it (`$CLAUDE_CONFIG_DIR` or `~/.claude`, `projects/*/<id>.jsonl`).
- **Codex**: launched with `-c notify=["<this binary>","hub","report","--source","codex","--event","notify"]`; its `thread-id` becomes `agent_session_id`.
- **Channel**: hooks report over `<state-dir>/report.sock` (unix socket, 0600, never on the network whatever `--bind` says; a state dir too long for a socket path uses `$TMPDIR/flywheel-hub-<uid>/<digest>.sock`, dir 0700) with a per-spawn random secret that authorizes status for that one session only. Reports are advisory UI state: they cannot start, stop, or change a session.
- **Not a sandbox**: the hub's token is never handed to agents (argv or env), but agents run as your user and can read `<state-dir>/token` like any of your files. Run untrusted agents under another user or in a container.

## Suggested commands

Agents draw suggested commands with margins and wrapping, so copying them off the terminal breaks them. The hub reads the same text from the agent's own record instead and serves each fenced code block exactly as written.

- **Sources**: Claude Code's transcript (`projects/*/<agent_session_id>.jsonl`, or the hooks' `transcript_path`), assistant text only (not thinking, tool output, or subagent sidechains); Codex's rollout (`$CODEX_HOME` or `~/.codex`, `sessions/YYYY/MM/DD/rollout-*-<thread-id>.jsonl`), or, when none is found, the `notify` payload's `last-assistant-message` (closed fences only: the report budget can cut it).
- **Tailing**: running agent sessions are checked every second (at once on a hook report) from a byte offset; lines over 8 MiB are skipped unread; a replaced or truncated file is re-read without duplicates; `/clear` (new conversation id) finishes the old file and follows the new one. The newest 500 blocks per session (and where reading stopped) are saved to `<state-dir>/commands/<session>.json`, so a restart keeps every block, pre-`/clear` ones included, under the same `seq`. A backlog read reports at most the newest 500. Codex rollout items carry no id: a block is keyed by its line's byte offset.
- **Shell blocks**: languages `bash sh zsh shell console shell-session fish ksh`, or untagged when the first non-comment line starts with a common command, a path (`./x`, `~/x`, `/x`), `*.sh`, `VAR=value`, or `$ `. `command` (what Copy/Send use) is `text`, except in `console`-style blocks (console languages, or untagged starting with `$ `): only the commands, `$ ` stripped, with continuations (after a trailing `\`, or `> ` lines, stripped) and the output lines left out.
- **Paste** (`Send`): bracketed paste when the target program enabled it (markers inside the text removed, newlines sent as CR), never Enter unless `run`. A multi-line paste into a terminal without bracketed paste is refused (409) unless `run` (use Copy there).
- CLI: on a terminal, control characters in a block print as `^[` etc. (piped output and `--json` are exact). A shell session resolves to its workspace's single agent session. `--copy` writes OSC 52 to the terminal (stdout, else `/dev/tty`) and a confirmation to stderr: works over SSH in terminals that allow it; tmux needs `set -g set-clipboard on`; under GNU screen (`$STY`) it is DCS-wrapped; large copies (over ~75 KB encoded) are dropped by some terminals.

## Auth

- Token: 256 random bits, hex, `<state-dir>/token` (0600). Rotate with `fw hub token --rotate`.
- One-time codes: 20 Crockford base32 characters (100 bits), single use, 5 minutes, at most 64 outstanding (a 65th drops the oldest, so a QR still on screen can stop working), digests only in memory, dropped on rotation. Minted by `serve`, `url`, or `POST /api/login-codes`. Case, `-`/spaces and `i l o` look-alikes are forgiven when typed.
- Browsers: `/login?code=...` or `/login?token=...` (or the form at `/`, whose one field takes either) sets an `HttpOnly; SameSite=Strict` cookie named `fw_hub_<digest of the token>` (hubs on one host do not clobber each other's login) (`Secure` behind an `X-Forwarded-Proto: https` trusted proxy or under an https `--public-url` / tailscale serve host). Its value is a fresh 256-bit per-login secret, never the token: digests in `<state-dir>/logins.json` (0600, survive restarts, 30 days, at most 256). `POST /logout` ends that login; rotation ends all. The signed-in page replaces itself with `/` (keeping `#session=`), so Back never revisits a spent code URL.
- Scripts: `Authorization: Bearer <token>` (codes are not bearer credentials).
- Failure limit: 10 wrong credentials (token, code, cookie, or bearer) per client address in a sliding 10 minutes, then everything that client presents gets 429 + `Retry-After` until the oldest failure ages out -- except a live login cookie, so an attacker sharing a NAT/proxy address cannot lock signed-in users out. IPv6 clients are keyed by /64. Direct connections from this machine (loopback or the host's own address, no `X-Forwarded-For`: the CLI, a local browser) are exempt, so a remote attacker cannot lock out `fw hub url`. Caveat: a local tunnel that adds no `X-Forwarded-For` (`ssh -L`, socat) makes its traffic look direct, hence exempt.
- Cross-origin `Origin` headers are refused on WebSocket upgrades and on every non-GET request.

## API

All under `/api`, JSON, token required. Errors are `{"error":"..."}`. `cols`/`rows` of 0 mean the default (120x32).

| Method | Path | |
|---|---|---|
| GET / POST | `/workspaces` | list / create `{"name","repos":[...]}` |
| GET / PATCH / DELETE | `/workspaces/{id}` | PATCH takes `name` and/or `repos` (409 if it drops a running session's repo); DELETE refuses (409) while sessions run |
| GET / POST | `/sessions[?workspace_id=]` | list / create `{"workspace_id","repo"?,"name"?,"kind"?,"command"?:[argv],"cols"?,"rows"?}` (`kind` defaults to `shell`; `command` only for shells, default `$SHELL`; default repo the first) |
| GET / PATCH / DELETE | `/sessions/{id}` | PATCH `{"name"}` renames; DELETE kills and forgets |
| POST | `/sessions/{id}/kill` | 202: SIGHUP the session (SIGKILL after 3s); record kept. 409 when nothing is left to kill |
| POST | `/sessions/{id}/restart` | relaunch an ended (`exited`/`stopped`) session in place: same id, name, command, repo; fresh PTY + scrollback. Body `{"cols"?,"rows"?}`. 409 while it runs |
| GET | `/sessions/{id}/screen[?source=visible\|recent&lines=N]` | rendered plain text (below) |
| POST | `/sessions/{id}/input` | `{"data","paste"?,"enter"?}` -> `{"bytes","bracketed"}`: type into it (`paste`: as `fw hub send`). 409 when it has no running terminal or is not reading |
| GET | `/sessions/{id}/commands[?limit=N&all=true]` | suggested code blocks, newest first (shell only unless `all`; `limit` 1..500, default 50): `{"session_id","conversation","transcript","commands":[{"seq","conversation","message_id","timestamp","block_index","lang","shell","text","command","source"}]}` |
| POST | `/sessions/{id}/paste` | `{"text","run"?}` -> `{"bracketed","ran","bytes"}`; 409 when not running, not reading, or unsafe multi-line |
| GET | `/sessions/{id}/ws` | WebSocket attach |
| GET | `/events[?client=ID]` | hub-wide events WebSocket (below) |
| GET | `/kinds` | `{"kinds":[{"kind","available","program"}]}`: which session kinds this host can launch |
| POST | `/login-codes` | `{"code","expires_in":300}`: a one-time code for `/login?code=` |
| POST | `/token/rotate` | 204: rotate the token (see `fw hub token --rotate`) |
| GET | `/repos/check?path=` | always 200 (even without `path`): `{"ok":true,"path":canonical}` or `{"ok":false,"error"}` (the new-workspace form validates as you type) |

WebSocket: server sends **text** `{"type":"snapshot","cols","rows","alt_screen","bytes"}`, then **binary** frames (the first `bytes` bytes are the snapshot, then live output), and **text** `{"type":"exit","code","signal"}` / `{"type":"reset"}` (fell behind: clear, a screen-only snapshot follows) / `{"type":"error","message"}`. Clients reset, size their terminal to `cols`x`rows`, replay the snapshot, then fit and send `resize` (the snapshot starts with RIS anyway). Client sends `{"type":"input","data":"..."}`, `{"type":"resize","cols":N,"rows":N}`, `{"type":"theme","foreground","background","cursor":"#rrggbb","ansi":[16 x "#rrggbb"]}` (the colours colour queries answer with), or binary raw input. Token rotation closes every socket with code 4001 (even one whose upgrade raced the rotation); the UI then shows sign-in with a "token was rotated" notice.

Session views carry `agent` (`{"status","detail"?,"since"}`, agent kinds only) and `agent_session_id`.

Events WebSocket (`client`: 1-64 of `[A-Za-z0-9_-]`, else 400; omitted, the connection gets its own): server sends `{"type":"snapshot","sessions":{id:status},"workspaces":{id:rollup}}` on connect and after falling behind, `{"type":"status","session_id","status"|null,"workspace_id","workspace_status"}` per change (as this client sees it), and `{"type":"sessions"}` when sessions or workspaces changed (re-list), and `{"type":"commands","session_id","fresh","commands":[...]}` when an agent suggested code blocks (newest first; `fresh`: written in the last 2 minutes, not a re-read backlog). Client sends `{"type":"view","session_id":id|null}` (what it shows; `null` while hidden). Token rotation closes it with 4001 too.

Screen text: `source=visible` (default) is the screen's rows (`lines` is refused); `recent` the last `lines` (1..100000, default 100) logical lines of scrollback + screen, soft wraps joined, trailing blank rows dropped. Both read the active screen.

```json
{"source":"visible","cols":80,"rows":24,"alt_screen":true,"title":"vim","cursor":{"row":2,"col":2,"visible":true},
 "modes":["app_cursor","bracketed_paste","mouse_drag","sgr_mouse"],"lines":["..."]}
```

```bash
T=$(fw hub token)
curl -s -H "Authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"name":"flywheel","repos":["/home/me/code/flywheel"]}' http://127.0.0.1:7420/api/workspaces
```

## Web UI

Sidebar: workspaces (collapsible, with an agent rollup chip: blocked / working / done) > sessions, each with its kind glyph and a status dot (live / done / failed / stopped / gone; ended sessions keep their exit code across hub restarts; a running agent shows working / blocked / done / idle instead, blocked with an alert edge). The tab title and favicon badges count blocked and done agents (`(1 blocked, 2 done) ...`; one favicon badge each); on narrow screens the drawer button badges agents in other sessions that need you. A blocked row shows what it asks under its name. Status and list changes are pushed over the events socket; the UI polls only while that socket is down. Dialogs create workspaces (repo paths checked live; paste several lines at once) and sessions (shell | claude | codex, repo picker defaulting to the attached session's repo). The attached session is kept in the URL (`#session=<id>`), so reloads re-attach; a dropped socket keeps the last screen, shows a countdown banner (and how many keystrokes were not sent), and reconnects with backoff (immediately on `online` / tab focus). An ended process, or a session whose terminal did not survive a hub restart, offers Relaunch (in place) and Close. Themes: auto (follows the OS), dark, light; the light theme lifts low-contrast agent TUI colours to 4.5:1.

| Key | |
|---|---|
| `Ctrl K` | command palette (fuzzy: sessions, actions, themes); again inside it sends a literal `^K` |
| `Alt N` / `Alt Shift N` | new session / new workspace |
| `Alt 1`..`9`, `Alt Up/Down` | jump to the nth / previous / next session with a terminal (numbers skip gone ones) |
| `Alt R`; `F2` or double-click on a sidebar row | rename session |
| `Alt W`; `Delete` on a focused sidebar row | close session (confirms while it runs) |
| `Alt C` / `Alt Shift C` | Commands drawer / copy the last suggested shell command |

The **Commands drawer** (`suggested` in the header, `Alt C`, or the palette; it overlays the terminal so opening it never resizes the PTY for other viewers) lists the attached agent's code blocks newest first (from a shell session: the workspace's agent that suggested last, or the one picked), unwrapped in monospace, each with Copy (the clipboard API on https/localhost, a selected-textarea fallback on plain-http LAN and iOS Safari), Send (paste into the chosen running shell of the workspace, no Enter) and Send + Run. Keys in the drawer: arrows / `j` `k` pick, `Enter` or `c` copy, `1`-`9` copy the nth, `s` Send, `Shift S` Send + Run, `Esc` close. Lines wider than the card fade at the edge with a `wide: scroll` hint. A new shell command from an agent in the workspace in view raises a toast with Copy.

Hub shortcuts are captured before the terminal sees them. On macOS the chord is `Ctrl Option` instead of `Alt`, so Option keeps typing layout characters (`| [ ] { }`, dead keys); elsewhere an `Alt` key the layout turns into a character also goes to the terminal. Served from the binary (no build step): xterm.js 6.0.0 + fit + web-links and the UI fonts (Big Shoulders Display, Martian Mono, JetBrains Mono; SIL OFL), vendored under `crates/flywheel-hub/web/vendor/` with their licenses. Only the fonts, icon, and `login.js` are served without the token (the login page uses them).

# Hub

`fw hub` is a web workspace for AI coding agents. The hub server owns every terminal: each session is a real PTY (shell, prompts, running processes, scrollback) that browsers attach to and detach from without disturbing it. Many clients can watch one session. Crate: `crates/flywheel-hub`.

**Security first:** a hub serves shells over HTTP -- anyone with the token has a shell as you. It binds `127.0.0.1` by default; for remote access put it behind `tailscale serve` rather than `--bind 0.0.0.0`.

## Verbs

```bash
fw hub serve [--bind ADDR] [--port N] [--public-url URL]... [--allow-origin ORIGIN]... [--trusted-proxy IP]... [--scrollback-lines N] [--state-dir DIR]
fw hub url [--qr] [--state-dir DIR]           # one-time login URLs for the running hub (+ QR for a phone)
fw hub token [--rotate] [--state-dir DIR]     # print the access token (created on first use) / rotate it
fw hub report --source claude|codex --event E [--detail D] [--stdin] [PAYLOAD]   # agent hooks only
```

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
- **Session** = one PTY process in one of its workspace's repos, of a **kind**: `shell` (the hub's `$SHELL`, or an explicit argv), `claude`, or `codex` (the plain CLI, resolved on the hub's `PATH`; a missing CLI is a 400). States: `running`, `exited` (code or signal), `stopped` (the hub stopped while it ran). An exited session keeps its scrollback attachable until removed or the hub restarts.
- Persisted to `<state-dir>/state.json` (0600, atomic writes). Processes are not restored across restarts yet.
- Killing a session hangs up its whole session (leader's group plus background jobs, even after the leader exited; Linux scans `/proc`, elsewhere only the leader's group). Removing a session, or deleting its workspace, also frees its PTY.
- Terminal size is shared: the last client to resize sets it for everyone.
- Input beyond 4 MiB queued for a terminal that is not reading is dropped (the client gets an `error` message).
- The server runs a terminal emulator per session (`alacritty_terminal`). Attaching sends a **snapshot** -- escape sequences that rebuild scrollback, screen, alternate screen (with the shell screen under it), cursor, saved cursor, SGR pen, scroll region, charsets, DEC modes (cursor keys, keypad, bracketed paste, mouse, focus), cursor style, title, palette -- then live bytes, with no gap or overlap. The snapshot carries the newest scrollback up to 2 MiB (whole lines); a client that falls behind gets the screen alone. Not replayed: custom tab stops, blink/overline; `?47`/`?1047` alt screens and the kitty keyboard protocol are not emulated.
- The server answers terminal queries (DA, DSR/cursor position, DECRQM, OSC 4/10/11/12 colours) once per session, attached or not; clients must not answer them. Colour answers use the viewer's theme: the web UI reports its palette per session on attach, theme change, and window focus (last report wins; the dark theme before any).
- Output cannot grow server memory without bound: OSC strings are cut at 64 KiB, titles at 4 KiB (title stack 10 deep), hyperlinks over 2 KiB dropped, at most 8 combining marks per cell.
- Sessions get `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`; agent sessions also `FLYWHEEL_HUB_REPORT_SOCKET` and `FLYWHEEL_HUB_REPORT_SECRET`. Markers of an enclosing Claude Code / Codex session (`CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ID`, `CODEX_SANDBOX`, ...) are removed, so a hub started from inside an agent does not make its agents think they are nested (Claude would stop saving transcripts).

## Agent status

| Status | Meaning | Claude Code | Codex |
|---|---|---|---|
| `working` | busy | `UserPromptSubmit`, `PreToolUse`, `PostToolUse[Failure]` | its busy line ("esc to interrupt") is drawn |
| `blocked` | waits on a person | `PermissionRequest`; `Notification` `permission_prompt` / `elicitation_dialog`; `idle_prompt` while working | -- |
| `done` | finished a turn **this client** has not viewed | `Stop`, `StopFailure` | `notify` `agent-turn-complete` |
| `idle` | finished and seen, or waiting for a first prompt | `SessionStart`; `done` once viewed | `done` once viewed |
| `unknown` | no signal yet, agent ended, or Codex's busy line went quiet 5 s with no turn-complete | `SessionEnd`, exit | exit |

- **Seen is per client** (a browser keeps an id in local storage): viewing a session (attached, page visible) turns its `done` into `idle` for that client only; a turn that finishes while a client watches is `idle` for it at once. The REST API counts a turn seen once any client viewed it.
- **Rollup**: a workspace shows its most urgent session: blocked > working > done > idle.
- **Claude**: the hub assigns `--session-id <uuid>` (stored as `agent_session_id`; follows `/clear`) and injects hooks with `--settings '<json>'` (merged with your own settings; `args` form, no shell) that run `<this binary> hub report ...`. Relaunching resumes the conversation (`--resume <id>`) once Claude has a transcript for it (`$CLAUDE_CONFIG_DIR` or `~/.claude`, `projects/*/<id>.jsonl`).
- **Codex**: launched with `-c notify=["<this binary>","hub","report","--source","codex","--event","notify"]`; its `thread-id` becomes `agent_session_id`.
- **Channel**: hooks report over `<state-dir>/report.sock` (unix socket, 0600, never on the network whatever `--bind` says) with a per-spawn random secret that authorizes status for that one session only. The hub's token is never given to agents. Reports are advisory UI state: they cannot start, stop, or change a session.

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
| GET | `/sessions/{id}/ws` | WebSocket attach |
| GET | `/events[?client=ID]` | hub-wide events WebSocket (below) |
| GET | `/kinds` | `{"kinds":[{"kind","available","program"}]}`: which session kinds this host can launch |
| POST | `/login-codes` | `{"code","expires_in":300}`: a one-time code for `/login?code=` |
| POST | `/token/rotate` | 204: rotate the token (see `fw hub token --rotate`) |
| GET | `/repos/check?path=` | always 200 (even without `path`): `{"ok":true,"path":canonical}` or `{"ok":false,"error"}` (the new-workspace form validates as you type) |

WebSocket: server sends **text** `{"type":"snapshot","cols","rows","alt_screen","bytes"}`, then **binary** frames (the first `bytes` bytes are the snapshot, then live output), and **text** `{"type":"exit","code","signal"}` / `{"type":"reset"}` (fell behind: clear, a screen-only snapshot follows) / `{"type":"error","message"}`. Clients reset, size their terminal to `cols`x`rows`, replay the snapshot, then fit and send `resize` (the snapshot starts with RIS anyway). Client sends `{"type":"input","data":"..."}`, `{"type":"resize","cols":N,"rows":N}`, `{"type":"theme","foreground","background","cursor":"#rrggbb","ansi":[16 x "#rrggbb"]}` (the colours colour queries answer with), or binary raw input. Token rotation closes every socket with code 4001 (even one whose upgrade raced the rotation); the UI then shows sign-in with a "token was rotated" notice.

Session views carry `agent` (`{"status","detail"?,"since"}`, agent kinds only) and `agent_session_id`.

Events WebSocket (`client`: 1-64 of `[A-Za-z0-9_-]`, else the connection gets its own): server sends `{"type":"snapshot","sessions":{id:status},"workspaces":{id:rollup}}` on connect and after falling behind, `{"type":"status","session_id","status"|null,"workspace_id","workspace_status"}` per change (as this client sees it), and `{"type":"sessions"}` when sessions or workspaces changed (re-list). Client sends `{"type":"view","session_id":id|null}` (what it shows; `null` while hidden). Token rotation closes it with 4001 too.

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

Sidebar: workspaces (collapsible, with an agent rollup chip: blocked / working / done) > sessions, each with its kind glyph and a status dot (live / done / failed / stopped / gone; ended sessions keep their exit code across hub restarts; a running agent shows working / blocked / done / idle instead, blocked with an alert edge). The tab title and favicon badge count blocked and done agents (`(1 blocked, 2 done) ...`). Status and list changes are pushed over the events socket; the UI polls only while that socket is down. Dialogs create workspaces (repo paths checked live; paste several lines at once) and sessions (shell | claude | codex, repo picker defaulting to the attached session's repo). The attached session is kept in the URL (`#session=<id>`), so reloads re-attach; a dropped socket keeps the last screen, shows a countdown banner (and how many keystrokes were not sent), and reconnects with backoff (immediately on `online` / tab focus). An ended process, or a session whose terminal did not survive a hub restart, offers Relaunch (in place) and Close. Themes: auto (follows the OS), dark, light; the light theme lifts low-contrast agent TUI colours to 4.5:1.

| Key | |
|---|---|
| `Ctrl K` | command palette (fuzzy: sessions, actions, themes); again inside it sends a literal `^K` |
| `Alt N` / `Alt Shift N` | new session / new workspace |
| `Alt 1`..`9`, `Alt Up/Down` | jump to the nth / previous / next session with a terminal (numbers skip gone ones) |
| `Alt R`; `F2` or double-click on a sidebar row | rename session |
| `Alt W`; `Delete` on a focused sidebar row | close session (confirms while it runs) |

Hub shortcuts are captured before the terminal sees them. On macOS the chord is `Ctrl Option` instead of `Alt`, so Option keeps typing layout characters (`| [ ] { }`, dead keys); elsewhere an `Alt` key the layout turns into a character also goes to the terminal. Served from the binary (no build step): xterm.js 6.0.0 + fit + web-links and the UI fonts (Big Shoulders Display, Martian Mono, JetBrains Mono; SIL OFL), vendored under `crates/flywheel-hub/web/vendor/` with their licenses. Only the fonts, icon, and `login.js` are served without the token (the login page uses them).

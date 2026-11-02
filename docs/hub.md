# Hub

`fw hub` is a web workspace for AI coding agents. The hub server owns every terminal: each session is a real PTY (shell, prompts, running processes, scrollback) that browsers attach to and detach from without disturbing it. Many clients can watch one session. Crate: `crates/flywheel-hub`.

**Security first:** a hub serves shells over HTTP -- anyone with the token has a shell as you. It binds `127.0.0.1` by default; for remote access put it behind `tailscale serve` rather than `--bind 0.0.0.0`.

## Verbs

```bash
fw hub serve [--bind ADDR] [--port N] [--allow-origin ORIGIN]... [--scrollback-lines N] [--state-dir DIR]
fw hub token [--state-dir DIR]     # print the access token (created on first use)
```

`serve` runs in the foreground and prints the login URL (`http://127.0.0.1:7420/login?token=...`; on `--bind 0.0.0.0` also a `from another device:` URL on the default-route address). Ctrl-C / SIGTERM records every running session stopped, then hangs them up (SIGHUP, SIGKILL after a grace).

| Flag | Default | |
|---|---|---|
| `--bind` | `127.0.0.1` | non-loopback prints a warning |
| `--port` | `7420` | `0` picks a free port |
| `--allow-origin` | none | extra browser origin (e.g. `https://box.tailnet.ts.net`); same-origin always allowed |
| `--scrollback-lines` | `5000` | per-session scrollback; capped at 1M cells (lines x cols), so wide terminals keep fewer |
| `--state-dir` | `$XDG_STATE_HOME/flywheel/hub`, else `~/.local/state/flywheel/hub` | |

## Remote access (phone, other machines)

```bash
fw hub serve                       # loopback only
tailscale serve --bg 7420          # https://<machine>.<tailnet>.ts.net -> 127.0.0.1:7420
```

Open `https://<machine>.<tailnet>.ts.net/login?token=$(fw hub token)` on the phone. If the proxy rewrites `Host`, add `--allow-origin https://<machine>.<tailnet>.ts.net`.

## Model

- **Workspace** = a name (unique) + 1..N git work-tree roots (absolute or `~/...`; a `.git` dir or linked-worktree `.git` file).
- **Session** = one PTY process in one of its workspace's repos, of a **kind**: `shell` (the hub's `$SHELL`, or an explicit argv), `claude`, or `codex` (the plain CLI, resolved on the hub's `PATH`; a missing CLI is a 400). States: `running`, `exited` (code or signal), `stopped` (the hub stopped while it ran). An exited session keeps its scrollback attachable until removed or the hub restarts.
- Persisted to `<state-dir>/state.json` (0600, atomic writes). Processes are not restored across restarts yet.
- Killing a session hangs up its whole session (leader's group plus background jobs, even after the leader exited; Linux scans `/proc`, elsewhere only the leader's group). Removing a session, or deleting its workspace, also frees its PTY.
- Terminal size is shared: the last client to resize sets it for everyone.
- Input beyond 4 MiB queued for a terminal that is not reading is dropped (the client gets an `error` message).
- The server runs a terminal emulator per session (`alacritty_terminal`). Attaching sends a **snapshot** -- escape sequences that rebuild scrollback, screen, alternate screen (with the shell screen under it), cursor, saved cursor, SGR pen, scroll region, charsets, DEC modes (cursor keys, keypad, bracketed paste, mouse, focus), cursor style, title, palette -- then live bytes, with no gap or overlap. The snapshot carries the newest scrollback up to 2 MiB (whole lines); a client that falls behind gets the screen alone. Not replayed: custom tab stops, blink/overline; `?47`/`?1047` alt screens and the kitty keyboard protocol are not emulated.
- The server answers terminal queries (DA, DSR/cursor position, DECRQM, OSC 4/10/11/12 colours) once per session, attached or not; clients must not answer them. Colour answers use the web UI's dark theme, even for a client on the light theme.
- Output cannot grow server memory without bound: OSC strings are cut at 64 KiB, titles at 4 KiB (title stack 10 deep), hyperlinks over 2 KiB dropped, at most 8 combining marks per cell.
- Sessions get `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`.

## Auth

- Token: 256 random bits, hex, `<state-dir>/token` (0600). Delete the file and restart to rotate.
- Browsers: `/login?token=...` (or the form at `/`) sets an `HttpOnly; SameSite=Strict` cookie named `fw_hub_<digest of the token>` (hubs on one host do not clobber each other's login) (`Secure` behind an `X-Forwarded-Proto: https` proxy).
- Scripts: `Authorization: Bearer <token>`.
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
| GET | `/kinds` | `{"kinds":[{"kind","available","program"}]}`: which session kinds this host can launch |
| GET | `/repos/check?path=` | always 200 (even without `path`): `{"ok":true,"path":canonical}` or `{"ok":false,"error"}` (the new-workspace form validates as you type) |

WebSocket: server sends **text** `{"type":"snapshot","cols","rows","alt_screen","bytes"}`, then **binary** frames (the first `bytes` bytes are the snapshot, then live output), and **text** `{"type":"exit","code","signal"}` / `{"type":"reset"}` (fell behind: clear, a screen-only snapshot follows) / `{"type":"error","message"}`. Clients reset, size their terminal to `cols`x`rows`, replay the snapshot, then fit and send `resize` (the snapshot starts with RIS anyway). Client sends `{"type":"input","data":"..."}`, `{"type":"resize","cols":N,"rows":N}`, or binary raw input.

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

Sidebar: workspaces (collapsible) > sessions, each with its kind glyph and a status dot (live / done / failed / stopped / gone; ended sessions keep their exit code across hub restarts). Dialogs create workspaces (repo paths checked live; paste several lines at once) and sessions (shell | claude | codex, repo picker defaulting to the attached session's repo). The attached session is kept in the URL (`#session=<id>`), so reloads re-attach; a dropped socket keeps the last screen, shows a countdown banner (and how many keystrokes were not sent), and reconnects with backoff (immediately on `online` / tab focus). An ended process, or a session whose terminal did not survive a hub restart, offers Relaunch (in place) and Close. Themes: auto (follows the OS), dark, light; the light theme lifts low-contrast agent TUI colours to 4.5:1.

| Key | |
|---|---|
| `Ctrl K` | command palette (fuzzy: sessions, actions, themes); again inside it sends a literal `^K` |
| `Alt N` / `Alt Shift N` | new session / new workspace |
| `Alt 1`..`9`, `Alt Up/Down` | jump to the nth / previous / next session with a terminal (numbers skip gone ones) |
| `Alt R`; `F2` or double-click on a sidebar row | rename session |
| `Alt W`; `Delete` on a focused sidebar row | close session (confirms while it runs) |

Hub shortcuts are captured before the terminal sees them. On macOS the chord is `Ctrl Option` instead of `Alt`, so Option keeps typing layout characters (`| [ ] { }`, dead keys); elsewhere an `Alt` key the layout turns into a character also goes to the terminal. Served from the binary (no build step): xterm.js 6.0.0 + fit + web-links and the UI fonts (Big Shoulders Display, Martian Mono, JetBrains Mono; SIL OFL), vendored under `crates/flywheel-hub/web/vendor/` with their licenses. Only the fonts and icon are served without the token (the login page uses them).

# Hub

`fw hub` is a web workspace for AI coding agents. The hub server owns every terminal: each session is a real PTY (shell, prompts, running processes, scrollback) that browsers attach to and detach from without disturbing it. Many clients can watch one session. Crate: `crates/flywheel-hub`.

**Security first:** a hub serves shells over HTTP -- anyone with the token has a shell as you. It binds `127.0.0.1` by default; for remote access put it behind `tailscale serve` rather than `--bind 0.0.0.0`.

## Verbs

```bash
fw hub serve [--bind ADDR] [--port N] [--allow-origin ORIGIN]... [--scrollback-bytes N] [--state-dir DIR]
fw hub token [--state-dir DIR]     # print the access token (created on first use)
```

`serve` runs in the foreground and prints the login URL (`http://127.0.0.1:7420/login?token=...`; on `--bind 0.0.0.0` also a `from another device:` URL on the default-route address). Ctrl-C / SIGTERM records every running session stopped, then hangs them up (SIGHUP, SIGKILL after a grace).

| Flag | Default | |
|---|---|---|
| `--bind` | `127.0.0.1` | non-loopback prints a warning |
| `--port` | `7420` | `0` picks a free port |
| `--allow-origin` | none | extra browser origin (e.g. `https://box.tailnet.ts.net`); same-origin always allowed |
| `--scrollback-bytes` | `2097152` | per-session output retained for replay |
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
- Replay after the ring fills starts at a line boundary; terminal modes set before that point (alt screen, colours) are not replayed.
- Sessions get `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`.

## Auth

- Token: 256 random bits, hex, `<state-dir>/token` (0600). Delete the file and restart to rotate.
- Browsers: `/login?token=...` (or the form at `/`) sets an `HttpOnly; SameSite=Strict` cookie (`Secure` behind an `X-Forwarded-Proto: https` proxy).
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
| GET | `/sessions/{id}/ws` | WebSocket attach |
| GET | `/kinds` | `{"kinds":[{"kind","available","program"}]}`: which session kinds this host can launch |
| GET | `/repos/check?path=` | always 200: `{"ok":true,"path":canonical}` or `{"ok":false,"error"}` (the new-workspace form validates as you type) |

WebSocket: server sends **binary** frames of raw output (scrollback replay first, then live) and **text** JSON `{"type":"exit","code","signal"}` / `{"type":"reset"}` (fell behind: clear, replay follows) / `{"type":"error","message"}`. Client sends `{"type":"input","data":"..."}`, `{"type":"resize","cols":N,"rows":N}`, or binary raw input.

```bash
T=$(fw hub token)
curl -s -H "Authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"name":"flywheel","repos":["/home/me/code/flywheel"]}' http://127.0.0.1:7420/api/workspaces
```

## Web UI

Sidebar: workspaces (collapsible) > sessions, each with its kind glyph and a status dot (live / done / failed / stopped / gone). Dialogs create workspaces (repo paths checked live; paste several lines at once) and sessions (shell | claude | codex, repo picker). The attached session is kept in the URL (`#session=<id>`), so reloads re-attach; a dropped socket keeps the last screen, shows a countdown banner, and reconnects with backoff (immediately on `online` / tab focus). An ended process offers Relaunch and Close. Themes: auto (follows the OS), dark, light.

| Key | |
|---|---|
| `Ctrl K` | command palette (fuzzy: sessions, actions, themes); again inside it sends a literal `^K` |
| `Alt N` / `Alt Shift N` | new session / new workspace |
| `Alt 1`..`9`, `Alt Up/Down` | jump to the nth / previous / next session |
| `Alt R`, `F2`, double-click | rename session |
| `Alt W`, `Delete` | close session (confirms while it runs) |

Hub shortcuts are captured before the terminal sees them. Served from the binary (no build step): xterm.js 6.0.0 + fit + web-links and the UI fonts (Big Shoulders Display, Martian Mono, JetBrains Mono; SIL OFL), vendored under `crates/flywheel-hub/web/vendor/` with their licenses. Only the fonts and icon are served without the token (the login page uses them).

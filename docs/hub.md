# Hub

`fw hub` is a web workspace for AI coding agents. The hub server owns every terminal: each session is a real PTY (shell, prompts, running processes, scrollback) that browsers attach to and detach from without disturbing it. Many clients can watch one session. Crate: `crates/flywheel-hub`.

**Security first:** a hub serves shells over HTTP -- anyone with the token has a shell as you. It binds `127.0.0.1` by default; for remote access put it behind `tailscale serve` rather than `--bind 0.0.0.0`.

## Verbs

```bash
fw hub serve [--bind ADDR] [--port N] [--allow-origin ORIGIN]... [--scrollback-bytes N] [--state-dir DIR]
fw hub token [--state-dir DIR]     # print the access token (created on first use)
```

`serve` runs in the foreground and prints the login URL (`http://127.0.0.1:7420/login?token=...`). Ctrl-C / SIGTERM hangs up every session (SIGHUP, SIGKILL after a grace) and records them stopped.

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

- **Workspace** = a name (unique) + 1..N git work-tree roots (absolute; a `.git` dir or linked-worktree `.git` file).
- **Session** = one PTY process in one of its workspace's repos. States: `running`, `exited` (code or signal), `stopped` (the hub stopped while it ran). An exited session keeps its scrollback attachable until removed or the hub restarts.
- Persisted to `<state-dir>/state.json` (0600, atomic writes). Processes are not restored across restarts yet.
- Sessions get `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`.

## Auth

- Token: 256 random bits, hex, `<state-dir>/token` (0600). Delete the file and restart to rotate.
- Browsers: `/login?token=...` (or the form at `/`) sets an `HttpOnly; SameSite=Strict` cookie (`Secure` behind an `X-Forwarded-Proto: https` proxy).
- Scripts: `Authorization: Bearer <token>`.
- Cross-origin `Origin` headers are refused on WebSocket upgrades and on every non-GET request.

## API

All under `/api`, JSON, token required.

| Method | Path | |
|---|---|---|
| GET / POST | `/workspaces` | list / create `{"name","repos":[...]}` |
| GET / PATCH / DELETE | `/workspaces/{id}` | PATCH takes `name` and/or `repos`; DELETE refuses (409) while sessions run |
| GET / POST | `/sessions[?workspace_id=]` | list / create `{"workspace_id","repo"?,"name"?,"command"?:[argv],"cols"?,"rows"?}` (default command `$SHELL`, default repo the first) |
| GET / DELETE | `/sessions/{id}` | DELETE kills and forgets |
| POST | `/sessions/{id}/kill` | SIGHUP the process group (SIGKILL after 3s); record kept |
| GET | `/sessions/{id}/ws` | WebSocket attach |

WebSocket: server sends **binary** frames of raw output (scrollback replay first, then live) and **text** JSON `{"type":"exit","code","signal"}` / `{"type":"reset"}` (fell behind: clear, replay follows) / `{"type":"error","message"}`. Client sends `{"type":"input","data":"..."}`, `{"type":"resize","cols":N,"rows":N}`, or binary raw input.

```bash
T=$(fw hub token)
curl -s -H "Authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"name":"flywheel","repos":["/home/me/code/flywheel"]}' http://127.0.0.1:7420/api/workspaces
```

## Web UI

Served from the binary (no build step): xterm.js 6.0.0 + fit + web-links, vendored under `crates/flywheel-hub/web/vendor/` with their licenses.

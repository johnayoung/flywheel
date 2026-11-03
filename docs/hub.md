# Hub

`fw hub` is a web workspace for AI coding agents. The hub server owns every terminal: each session is a real PTY (shell, prompts, running processes, scrollback) that browsers attach to and detach from without disturbing it. Many clients can watch one session. Crate: `crates/flywheel-hub`.

**Security first:** a hub serves shells over HTTP -- anyone with the token has a shell as you. It binds `127.0.0.1` by default; for remote access put it behind `tailscale serve` rather than `--bind 0.0.0.0`.

## Verbs

```bash
fw hub start [SERVE FLAGS] [--timeout SECS]   # detached; returns once serving, prints login URLs
fw hub stop [--timeout SECS] [--force]        # graceful: sessions recorded for restore
fw hub upgrade [--binary PATH] [--allow-restart] [--timeout SECS]   # live handoff to a new binary; no session restarts (below)
fw hub status [--json]                        # pid, uptime, url, counts; exit 0 running, 3 not, 1 unreachable
fw hub logs [-f] [-n N]                       # <state-dir>/hub.log (detached hubs only)
fw hub install-service [SERVE FLAGS] [--name N] [--force] [--print]   # systemd --user unit; never enabled
fw hub serve [SERVE FLAGS]                    # foreground
fw hub url [--qr]                             # one-time login URLs for the running hub (+ QR for a phone)
fw hub open [--session S]                     # open a browser here, signed in (fresh one-time code; URL printed too)
fw hub token [--rotate]                       # print the access token (created on first use) / rotate it
fw hub commands SESSION [--last] [--all] [--json] [--copy] [--limit N]   # agent-suggested code blocks
fw hub report --source claude|codex --event E [--detail D] [--stdin] [PAYLOAD]   # agent hooks only
# start/serve also take --handoff (take over from the running hub; --bind/--port ignored)
# SERVE FLAGS: [--bind ADDR] [--port N] [--public-url URL]... [--allow-origin ORIGIN]... [--trusted-proxy IP]... [--scrollback-lines N] [--no-restore] [--no-agent-resume] [--no-flywheel] [--import-root DIR] [--notify-url URL] [--worktree-branch-prefix P]
# every verb but report (hooks only) takes --state-dir DIR
# ls/new/send/read/wait/attach/kill/relaunch/commands/open/url also take [--url URL] [--token T]: a hub elsewhere
#   (token: prefer FW_HUB_TOKEN; without one, the state dir's token goes only to that state dir's running hub)
```

Scripting the running hub, or the one `--url` names (sessions/workspaces by id, unique id prefix, or name; exact name beats prefix; ambiguity is an error):

```bash
fw hub ls [--json]                                         # workspaces > sessions: short id, kind, state (+ agent status), name, live cwd [branch +ahead -behind ~changed ?untracked]
fw hub new WS [--kind shell|claude|codex] [--folder PATH|LABEL] [--name N] [--create] [--worktree TASK [--base REF]] [--json] [-- ARGV...]   # prints the id; a taken --name is refused (--repo = --folder)
fw hub send SESSION TEXT|- [--enter] [--raw]               # pasted: newlines -> CR, bracketed when the program enabled it
fw hub read SESSION [--source visible|recent] [--lines N] [--json]
fw hub wait SESSION --until done|blocked|idle|working|exit [--timeout SECS] [--changed]   # exit 0 met, 124 timeout, 1 error
fw hub attach [SESSION] [--prefix C-b] [--no-status]      # this terminal on a session (picker without SESSION); see Terminal client
fw hub kill SESSION [--rm [--remove-worktree [--force]]]   # hang up (record kept, as exited); --rm also forgets it; see Worktrees
fw hub relaunch SESSION                                    # start an exited/stopped session again in place
fw hub import FILE.code-workspace... [--name N] [--update] [--json]   # VS Code multi-root workspaces -> workspaces
```

`import` reads VS Code workspace files (JSONC: comments, trailing commas): each `folders` entry's `path` (relative to the file's directory, absolute, or `~/`) or `file://` `uri` (and, on WSL, `vscode-remote://wsl+<this distro>/...`) becomes a folder, its `name` the folder's label. The workspace is named after the file (`personal.code-workspace` -> `personal`) unless `--name` (one file only). Missing folders and entries naming no local directory (other remotes, Windows paths) are listed as skipped (stderr summarizes); a file with nothing importable fails. An existing workspace of that name is refused unless `--update`, which replaces its folders and labels (refused when that drops a folder a running session is in). Every file is tried; exit 1 if any failed.

`open` mints a one-time code for `http://<loopback>:<port>/login?code=...[#session=<id>]` (under `--url`: that URL's `/login?code=...`; `url --url` prints the same), prints that URL, and writes a 0600 redirect page (`open-<id>.html` in the state dir; expired ones are swept) that forwards to it -- the code never goes on an opener's argv (`/proc/*/cmdline` is world-readable). The page goes to `$BROWSER` (`:`-separated commands, `%s` = its `file://` URL) when set, else on WSL `wslview`, else `cmd.exe /c start` (its `\\wsl.localhost\...` path, `^`-escaped), else `xdg-open` (macOS: `open`). Only an opener that cannot start falls through to the next; one that runs and fails ends it (exit 1: it may have opened a tab, and the code is single-use).

`new --create` makes workspace `WS` (with `--folder`) when none matches; `--folder` is a path (relative to the caller's directory, and on the hub's machine under `--url`) or one of the workspace's folder labels or directory names (`--folder john-finances`). `send` wraps text in `ESC[200~ ... ESC[201~` only when the program set bracketed paste (markers inside the text are stripped, repeatedly, so split markers cannot reassemble), a final newline becomes that Enter (`printf 'make\n' | fw hub send s -` runs it), and sends Enter 50 ms later as its own keypress; `--raw` sends bytes untouched (`fw hub send s $'\x03' --raw` is Ctrl-C).

## Terminal client: `fw hub attach`

Any terminal (local, over SSH, or a remote hub via `--url https://box.tailnet.ts.net` + `FW_HUB_TOKEN`) attaches to the same server-owned sessions the browser does. `--url` needs `--token`/`FW_HUB_TOKEN`; the state dir's token is only sent when `--url` is that state dir's running hub.

- No SESSION: a picker of workspaces > sessions with live status (`j`/`k`, Enter, `/` filter by name, kind, or workspace, `q`; Esc clears a filter, else returns to the session it was opened from; `q` there detaches). Stopped sessions are listed but not attachable (`fw hub relaunch`); make and end sessions with `fw hub new|kill` or the web UI.
- Attached: the hub's snapshot, then raw live bytes; typed bytes go verbatim (keys, mouse, bracketed paste). The PTY follows this terminal's size (SIGWINCH; typing takes the size back from another viewer). Counts as viewing (a finished turn is `idle` for it, as for a browser tab); the client id is stable per user and machine, so a turn seen in one attach stays seen in the next. Output is flow-controlled (at most 256 KiB in flight): a slow terminal makes the hub skip ahead with a fresh snapshot rather than queue.
- The session's process ending while attached ends the client (exit 0, `[ws/name exited N]`, its last screen left in place); attaching to an already-ended session shows its last screen until `C-b d`.
- Prefix (`C-b`, `--prefix C-a`): `d` detach (session keeps running), `s` switch (picker), `t` status bar on/off, `r` redraw (fresh snapshot), `?` key table (full screen; any key returns), prefix twice sends it. Inside a bracketed paste the prefix is pasted.
- Status bar (last row; the session gets one row less, below its scroll region): `workspace/session [status]`, hub-wide `N blocked  N done`. `--no-status` starts without it.
- Terminal queries in the output (DA, DSR/CPR, DECRQM, DECRQSS, XTVERSION, XTGETTCAP, OSC 4/10-19/52 reads, size reports, ENQ, kitty keyboard) are not passed to this terminal: the hub answers them.
- Token rotation: with the token from the state dir the client re-reads it and reconnects; a hand-given token ends it (exit 1).
- The terminal is restored on detach, errors, hub stop, SIGTERM/SIGHUP, and panics.

## Scripting agents

`send` types, `wait` blocks on the events socket (no polling), `read` / `commands` read back. `wait --until idle` accepts `done`; `--changed` ignores the state current at start (the previous turn's `done`); an agent wait fails (1) if the session ends first; `--until exit` prints `exited N` / `killed (SIG)`. `--timeout` bounds everything, connecting included, but the state is always checked once (`--timeout 0`: check and return; reaching the hub may take up to 1 s).

```bash
id=$(fw hub new demo --kind claude --name fixer)
fw hub wait "$id" --until idle --timeout 60           # agent up, waiting for a prompt
fw hub send "$id" $'Fix the failing test in src/lib.rs\n'
fw hub wait "$id" --until done --changed --timeout 900 || { fw hub read "$id"; exit 1; }
fw hub commands "$id" --last > next.sh                # the newest shell block it suggested, exactly
fw hub send sh "$(cat next.sh)" --enter               # run it in an interactive shell session
fw hub new demo --name build -- sh -c "$(cat next.sh)"   # or as a one-shot session...
fw hub wait build --until exit --timeout 600          # ...and wait for it to end (prints `exited N`)
fw hub wait "$id" --until blocked --timeout 5 && fw hub read "$id"   # 0: it waits on a permission prompt
```

## Lifecycle and restore

- `start` re-execs `fw hub serve` in a new session (`setsid`, cwd `/`, stdin `/dev/null`), stdout/stderr into `<state-dir>/hub.log` (0600; rotated at 10 MiB, 3 kept as `hub.log.1..3`). Ready = it wrote `server.json` with its pid (after binding and restoring). Failing to start prints the log tail; not ready within `--timeout` (30 s) it is SIGTERMed. Already running: prints it, exits 0. One-time codes and the token never go to the log.
- `stop` sends SIGTERM (asking the hub nothing first) and waits up to `--timeout` for the serve lock to free; no hub running exits 0. `status` waits at most 3 s for the hub to answer. `--force` SIGKILLs after `--timeout` (sessions then not recorded for restore).
- **Restore** (on every start unless `--no-restore`): sessions recorded `stopped` by a clean shutdown come back with the same id, name, and order, a fresh PTY in the directory their process was in at shutdown (Linux `/proc`; else, or when gone, the session's repo). Interactive shells (`sh`, `bash`, `zsh`, `fish`, ... with at most `-l`/`-i`) relaunch as recorded. Agents resume their conversation in their repo, with fresh status hooks and secret: `claude --resume <agent_session_id>` once its transcript exists, `codex -c notify=[...] resume <thread-id>` once a rollout for the reported thread id exists (`$CODEX_HOME` or `~/.codex`, `sessions/YYYY/MM/DD/rollout-*-<id>.jsonl`, newest 400 days). Anything else -- a shell running a command, an agent with nothing to resume, any agent under `--no-agent-resume` -- comes back as a fresh hub shell (never re-runs the command; the per-kind decision is `restore::plan`). A fallback keeps the record's `kind`/`command` and sets `fallback_command` (cleared by a relaunch). A session whose repo dir is gone stays `stopped` with `restore_error` (retried each start; UI and `ls` show "not restored"); a gone `last_cwd` restores in the repo with a note in the log. Sessions found `running` after a crash get `restore_error` "not shut down cleanly" and are not restored (`start` lists them; `fw hub relaunch`). `--no-restore` drops pending restores for good (later starts do not restore them). SIGTERM during startup is held until serving, then shuts down cleanly. Creating or relaunching a session whose directory is missing is a 400, never a shell in `$HOME`.
- `install-service` writes `$XDG_CONFIG_HOME/systemd/user/<name>.service` (`~/.config/...`): `ExecStart=<this binary> hub serve ...` (absolute state dir), current `PATH`/`SHELL`, `Restart=on-failure`, `KillMode=mixed` (SIGTERM reaches the hub first, so sessions are recorded for restore). It passes `--no-login-code`: no one-time code is written to the journal; use `fw hub url`. Prints the `systemctl --user enable --now` / `loginctl enable-linger` steps; enables nothing.

### Live handoff (`fw hub upgrade`)

Replaces the running hub process with a new one (default binary: `fw` on `PATH`, else this one) **without ending any session**: same processes, pids, screens, scrollback, port, agent status/seen state, and hook secrets. Use it after every rebuild instead of stop/start. Linux; exit 0 handed off, 4 refused (nothing changed), 3 no hub.

- Mechanism: the hub listens on `<state-dir>/handoff.sock` (0600, same-user peers only). `upgrade` asks it for its serve flags and runs `<binary> hub start --handoff <flags>`, i.e. `serve --handoff` detached. The old hub freezes (stops accepting -- connections wait in the listen backlog, so the port never closes; open connections get 503; PTYs paused, typed input held in order; reports paused; transcript offsets saved), passes the listening socket, serve lock, report socket, and per session the PTY master + pidfd + full emulator snapshot over `SCM_RIGHTS`, and exits only after the new hub acks `started`. Browsers' sockets drop and reconnect to the new process; `fw hub attach` detaches ("connection lost") and must be re-run.
- Failure: anything before the commit (refusal, error, timeout, new hub dying) thaws the old hub, which keeps serving; resizes sent during the freeze are dropped. New hub building fails -> it tells the old one and exits; the old hub vanishing after the new one is ready -> the new one takes over.
- Versioned protocol (`handoff::PROTOCOL`): a mismatch, a hub predating handoff, or a binary without `--handoff` is refused with the reason; `--allow-restart` then falls back to `stop` + `start` (restore path: processes restarted). A hub running as a systemd unit's main process refuses (systemd would kill the unit's cgroup, sessions included, when the old process exits): use `systemctl --user restart`.
- Handed-over processes are no longer the hub's children: their exit is seen via the pidfd (Linux 5.3+; else polling the pid) and recorded with **unknown exit code** (`exited ?` in `ls`, "exit code unknown" in the UI). The new hub runs no restore pass (stopped sessions wait for the next normal start). macOS: compiles, polls pids; untested.

`serve` runs in the foreground and prints one-time login URLs (below); when any is reachable from another device (non-loopback bind, `--public-url`, or a detected `tailscale serve`) it also draws a QR code of the first (only when stdout is a terminal; logs get a pointer to `url --qr`). One hub per state dir: the hub holds an exclusive lock on `<state-dir>/serve.lock` for its whole life (guarding `server.json` and `report.sock`), and `serve` refuses to start while another holds it. Ctrl-C / SIGTERM records every running session stopped, then hangs them up (SIGHUP, then SIGKILL to anything left in each session, `nohup`ed children too, before the process exits). While it runs, `<state-dir>/server.json` (0600) records its pid, address, and public URLs for `url` / `token --rotate`; one left behind by a crashed hub (lock not held) is ignored and removed, so the CLI never sends the token to whatever now listens there.

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
| `--notify-url` | none | ntfy-compatible push on blocked/done (below); overrides `url` in `<state-dir>/notify.toml` |
| `--scrollback-lines` | `5000` | per-session scrollback; capped at 1M cells (lines x cols), so wide terminals keep fewer |
| `--worktree-branch-prefix` | none | session worktree branches are `<prefix><task>` (e.g. `agent/`) |
| `--import-root` | `~/code` | where the UI's VS Code import looks for `*.code-workspace` (3 levels deep; hidden, `node_modules`/`target`/... and symlinked dirs skipped) |
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

## Push notifications

The hub POSTs to an ntfy-compatible webhook when an agent becomes **blocked** or finishes a turn (**done**), so a phone hears about it with every browser closed.

```bash
fw hub start --notify-url https://ntfy.sh/<long-random-topic>    # subscribe to the topic in the ntfy app
```

`<state-dir>/notify.toml` (must be `chmod 600`: the topic and token are secrets; `--notify-url` overrides `url` but shows in `ps` and in an `install-service` unit, so prefer the file):

```toml
url = "https://ntfy.example.com/hub"
token = "tk_..."               # optional, sent as Authorization: Bearer (or user:pass@ in the URL: Basic)
events = ["blocked", "done"]   # default both
debounce_secs = 30             # one push per session and status per window (a new spell inside it waits for its end)
settle_secs = 3                # how long a status must hold before it pushes (max 60)
detail = false                 # names only: leave out what a blocked agent asks (default true)
```

- Says: title `<session> is blocked|done`, body `<workspace> / <session> (<kind>) waits on you: <status detail>` / `... finished its turn.` (a worktree session: `(<kind> on <branch>)`), `Priority: high` for blocked, `Tags: flywheel,<status>`, `Click: <public URL>/#session=<id>` when the hub has a `--public-url` or `tailscale serve` URL. Never terminal content, prompts, paths, or tokens; logs name only the webhook's host.
- When: the status must hold 3 s (an approval within that never pushes), nobody may be viewing the session (a page counts as viewing while visible, focused, and used in the last 5 min; when the last viewer looks away or leaves, an unanswered spell pushes then), a finished turn must still be unseen. Each spell pushes at most once; a new spell within `debounce_secs` of the last push of that status is held until the window ends and pushes if it still stands. One HTTP/1.1 POST, 10 s timeout, no redirects followed, `HTTP(S)_PROXY` not used, rustls (ring) with the system's roots; failures go to the hub log.
- `GET /api/notify` says whether it is on (host only), `POST /api/notify/test` sends a test (palette: "Send a test push").

## Model

- **Workspace** = a name (unique) + 1..32 **folders**: existing directories (absolute or `~/...`), each a git work-tree root (`.git` dir or linked-worktree `.git` file) or a plain directory, with an optional display label. Each folder carries `git` and `missing` (both re-read from disk on load and on every listing); repo-only features (worktrees, branch badges) apply only where `git` is true. A `missing` folder (moved, deleted, unmounted) refuses sessions saying so, and may stay in an edit until removed. JSON: `folders: [{"path","name"?,"git","missing"}]` plus a legacy `repos` mirror of every path; input takes `folders` (paths or `{"path","name"}`; a path without `name` keeps its label, `""` clears it) or the legacy `repos`.
- **Session** = one PTY process in one of its workspace's folders, of a **kind**: `shell` (the hub's `$SHELL`, or an explicit argv), `claude`, or `codex` (the plain CLI, resolved on the hub's `PATH`; a missing CLI is a 400). States: `running`, `exited` (code or signal), `stopped` (the hub stopped while it ran; `clean` when by its own shutdown, plus `last_cwd` / `restore_error`). An exited session keeps its scrollback attachable until removed or the hub restarts.
- Persisted to `<state-dir>/state.json` (0600, atomic writes; schema version 2, version 1 files migrate on load, newer ones are refused). Restarts restore sessions (above) and keep each workspace's layout (below).
- Killing a session hangs up its whole session (leader's group plus background jobs, even after the leader exited; Linux scans `/proc`, elsewhere only the leader's group). Removing a session, or deleting its workspace, also frees its PTY.
- Terminal size is shared (at most 500x300): the last client to resize sets it for everyone, and every attached client is told (`size`). In the web UI only a pane that has focus, or whose session this browser alone views, resizes it; any other pane follows the size someone else set (clipped to its box, bottom rows in view, `another viewer's size` in its title bar) until it gets focus. So a small unfocused pane never shrinks a session another viewer is using.
- Input beyond 4 MiB queued for a terminal that is not reading is dropped (the client gets an `error` message).
- The server runs a terminal emulator per session (`alacritty_terminal`). Attaching sends a **snapshot** -- escape sequences that rebuild scrollback, screen, alternate screen (with the shell screen under it), cursor, saved cursor, SGR pen, scroll region, charsets, DEC modes (cursor keys, keypad, bracketed paste, mouse, focus), cursor style, title, palette -- then live bytes, with no gap or overlap. The snapshot carries the newest scrollback up to 2 MiB (whole lines); a client that falls behind gets the screen alone. Not replayed: custom tab stops, blink/overline; `?47`/`?1047` alt screens and the kitty keyboard protocol are not emulated.
- The server answers terminal queries (DA, DSR/cursor position, DECRQM, OSC 4/10/11/12 colours) once per session, attached or not; clients must not answer them. Colour answers use the viewer's theme: the web UI reports its palette per session on attach, theme change, and window focus (last report wins; the dark theme before any).
- Output cannot grow server memory without bound: OSC strings are cut at 64 KiB, titles at 4 KiB (title stack 10 deep), hyperlinks over 2 KiB dropped, at most 8 combining marks per cell.
- Sessions get `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`; agent sessions also `FLYWHEEL_HUB_REPORT_SOCKET` and `FLYWHEEL_HUB_REPORT_SECRET`. Markers of an enclosing Claude Code / Codex session (`CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ID`, `AI_AGENT`, `CODEX_SANDBOX`, ...) are removed, so a hub started from inside an agent does not make its agents think they are nested (Claude would stop saving transcripts).

## Worktrees and companion shells

- **Worktree sessions** (git folders only): `fw hub new WS --worktree TASK [--base REF]` or the new-session form's "in a new sibling worktree" runs `git worktree add -b <prefix>TASK <main-parent>/<repo>.worktrees/TASK <base>` and starts the session there (named TASK unless `--name`). `<main-parent>/<repo>` is the main checkout even when the folder is itself a linked worktree; a path inside a repo, an existing directory or branch, or a bad task name (`[A-Za-z0-9._-]`, max 64) is refused before git runs. Base: `REF`, else the repo's current branch (detached: its commit). A failed launch removes the fresh worktree.
- The record keeps `worktree: {repo, path, branch, base}`; `cwd` is the worktree. Restore and relaunch keep it there (worktree deleted meanwhile: `restore_error`). The workspace folder is `repo`.
- **Badges**: every session in a git directory shows branch, ahead/behind (worktree: vs its base; else the upstream), changed and untracked files. A background thread reads them (`git status`, `rev-list`): at start, on agent status changes, ~1.5 s and 4.5 s after a command is run by `send --enter` / Send + Run, and every 20 s for running sessions. Listing never runs git.
- **Close**: `kill --rm --remove-worktree` (UI: Close offers keep / remove) also closes the session's companion shells and runs `git worktree remove`. Refused (409, nothing closed) while another session (running or not) uses it, and -- unless `--force` -- when it has uncommitted changes, untracked files, or commits its base lacks (base deleted: commits no other local branch has). The branch is deleted only when nothing would be lost; forced removal of an unmerged branch keeps the branch. A plain close keeps the worktree. Only the agent's close removes it (400 on a companion).
- **Companion shell**: an agent's linked shell in its directory (worktree included), `companion_of: <agent id>`, listed right after it. Created on demand (`Alt \``, the drawer's Open shell, `POST /api/sessions/{id}/companion`); an ended one is relaunched in place, so there is one per agent. Closing the agent closes it too. Suggested-command Send targets it by default. The header's `shell` / `agent` pill flips between them (for phones); `ls` marks it `(companion of AGENT)`.
- **Readiness** before pasting into a new companion: bracketed paste enabled (bash 5.1+, zsh), else output then 500 ms quiet, else after 5 s anyway (typed ahead; `ready: "timeout"`).

## Layout: tabs and split panes

Each workspace has a **layout**: named tabs, each a tree of splits (side by side or stacked, any ratio) whose leaves are panes showing one session. Stored on the hub (`state.json`, per workspace, revisioned), shared by every browser; which tab is active, the focused pane, and zoom are per browser.

- **Desktop**: a tab bar over the stage (`+` new tab, double-click renames, right-click menu, middle-click closes); each pane has its own terminal, socket, reconnect banner, and title bar (split right / down, zoom, menu, close). Click or type in a pane to focus it (signal ring). Drag a divider (or focus it and use arrows, `Shift` for 10%, double-click for 50/50). Drag a session from the sidebar onto a pane: an edge splits that way, the middle shows it there. Opening a session focuses the pane already showing it (switching tabs), else shows it in the focused pane; the new-session form's **Show in** picks focused pane / split right / split down / new tab.
- **Splitting** with a new shell (`Alt \`, `Alt -`, title bar, menu): an agent's companion shell first (its directory, worktree included), else a shell in the session's folder.
- **Companion flip** (``Alt ` ``) on screens over 1100 px opens the companion beside the agent (focuses it when on screen; from a zoomed pane the zoom moves to it); palette "Companion shell: in place" restores the old swap.
- **Minimum pane size**: a split never leaves a pane under 40x8 cells. Without room the new session opens in a new tab (toast says so); the companion flip swaps in place instead.
- **Closing**: a pane (`Alt X`) only leaves the layout; its session keeps running. The last pane closes its tab (an only tab stays, empty). Closing or removing a session closes every pane showing it (the hub prunes layouts; pushed as `layout`).
- **Cost**: only the active tab's panes are attached (other tabs: no socket, no terminal; switching re-attaches from the snapshot); panes behind a zoom stay attached but are not drawn. xterm's DOM renderer, no WebGL/canvas contexts. At most 12 tabs per workspace and 12 panes per tab.
- **Phones** (under 760 px): no splits. One pane full screen; a chip row switches between the layout's sessions (one chip per session, `tab session`, the current one scrolled into view); phones never change layouts.
- **Seen**: every pane on screen counts as viewed (`view` carries `visible`); panes behind a zoom or in other tabs do not.
- **PTY size** (last resizer wins): a pane resizes its session's PTY only while focused or while it is the session's only viewer, once a divider drag ends (not per move). Otherwise it follows the other viewer's size (`NNxNN` badge), clipped to its box with the window kept on the cursor and `rows above` / `rows below` marks.
- **Restore**: a page opened without `#session=` reopens the last workspace this browser viewed (else the first with a saved layout) at its focused pane. An empty pane (new tab) offers the workspace's sessions and a new shell.
- **Browser check** (opt-in, not in `scripts/check.sh`): `node crates/flywheel-hub/e2e/layout.mjs [OUT_DIR]` -- temp state dir, free port, fake `claude`; 1440x900 3 panes + 2 tabs + reload + minimum pane size, a second smaller viewer, 390x844 phone landing, quick-answer parsing, tap targets (uncovered, 44px); screenshots in `OUT_DIR`. Env `FW`, `PLAYWRIGHT` (package path), `CHROMIUM` (executable).

| Key | |
|---|---|
| `Alt \` / `Alt -` | split the focused pane right / down with a new shell |
| `Alt Shift` arrows | focus the pane in that direction |
| `Alt Shift H J K L` | move the focused pane's nearest divider left / down / up / right |
| `Alt Z` / `Alt X` | zoom (this browser) / close pane |
| `Alt T`, `Alt PgUp/PgDn` | new tab, previous / next tab |
| `Ctrl B` then a key | only with palette "Pane prefix Ctrl B: on": `%` `"` split, arrows focus, `Ctrl` arrows resize, `z` `x` `c` `n` `p` `1`-`9` `,` (rename) `&` (close tab) `o` (next pane) `?`; `Ctrl B` twice sends `^B` |

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

- **Sources**: Claude Code's transcript (`projects/*/<agent_session_id>.jsonl`, or the hooks' `transcript_path`), assistant text only (not thinking, tool output, or subagent sidechains); Codex's rollout (`$CODEX_HOME` or `~/.codex`, `sessions/YYYY/MM/DD/rollout-*-<thread-id>.jsonl`, newest day first, at most 400 days back; restore's resume check uses the same search), or, when none is found, the `notify` payload's `last-assistant-message` (closed fences only: the report budget can cut it).
- **Tailing**: running agent sessions are checked every second (at once on a hook report) from a byte offset; lines over 8 MiB are skipped unread; a replaced or truncated file is re-read without duplicates; `/clear` (new conversation id) finishes the old file and follows the new one. The newest 500 blocks per session (and where reading stopped) are saved to `<state-dir>/commands/<session>.json`, so a restart keeps every block, pre-`/clear` ones included, under the same `seq`, and a resumed agent (restore) is followed on from where reading stopped. A session restored as a fallback shell is not followed (its saved blocks stay listed). A backlog read reports at most the newest 500. Codex rollout items carry no id: a block is keyed by its line's byte offset.
- **Shell blocks**: languages `bash sh zsh shell console shell-session fish ksh`, or untagged when the first non-comment line starts with a common command, a path (`./x`, `~/x`, `/x`), `*.sh`, `VAR=value`, or `$ `. `command` (what Copy/Send use) is `text`, except in `console`-style blocks (console languages, or untagged starting with `$ `): only the commands, `$ ` stripped, with continuations (after a trailing `\`, or `> ` lines, stripped) and the output lines left out.
- **Paste** (`Send`, `POST /sessions/{id}/paste`): the same encoding as `fw hub send` (`/input` with `paste`): bracketed when the target program enabled it (markers inside the text removed, newlines sent as CR), trailing newlines dropped, Enter (50 ms later, as its own keypress) only with `run`. A multi-line paste into a terminal without bracketed paste is refused (409) unless `run` (use Copy there).
- CLI: on a terminal, control characters in a block print as `^[` etc. (piped output and `--json` are exact). A shell session resolves to its workspace's single agent session. `--copy` writes OSC 52 to the terminal (stdout, else `/dev/tty`) and a confirmation to stderr: works over SSH in terminals that allow it; tmux needs `set -g set-clipboard on`; under GNU screen (`$STY`) it is DCS-wrapped; large copies (over ~75 KB encoded) are dropped by some terminals.

## Auth

- Token: 256 random bits, hex, `<state-dir>/token` (0600). Rotate with `fw hub token --rotate`.
- One-time codes: 20 Crockford base32 characters (100 bits), single use, 5 minutes, at most 64 outstanding (a 65th drops the oldest, so a QR still on screen can stop working), digests only in memory, dropped on rotation. Minted by `serve`, `url`, or `POST /api/login-codes`. Case, `-`/spaces and `i l o` look-alikes are forgiven when typed.
- Browsers: `/login?code=...` or `/login?token=...` (or the form at `/`, whose one field takes either) sets an `HttpOnly; SameSite=Strict` cookie named `fw_hub_<digest of the token>` (hubs on one host do not clobber each other's login) (`Secure` behind an `X-Forwarded-Proto: https` trusted proxy or under an https `--public-url` / tailscale serve host). Its value is a fresh 256-bit per-login secret, never the token: digests in `<state-dir>/logins.json` (0600, survive restarts, 30 days, at most 256). `POST /logout` ends that login and closes the sockets it opened (code 4002; also on expiry or eviction); rotation ends all. The signed-in page replaces itself with `/` (keeping `#session=`), so Back never revisits a spent code URL.
- Scripts: `Authorization: Bearer <token>` (codes are not bearer credentials).
- Failure limit: 10 wrong credentials (token, code, cookie, or bearer) per client address in a sliding 10 minutes, then everything that client presents gets 429 + `Retry-After` until the oldest failure ages out -- except a live login cookie, so an attacker sharing a NAT/proxy address cannot lock signed-in users out. IPv6 clients are keyed by /64. Direct connections from this machine (loopback or the host's own address, no `X-Forwarded-For`: the CLI, a local browser) are exempt, so a remote attacker cannot lock out `fw hub url`. Caveat: a local tunnel that adds no `X-Forwarded-For` (`ssh -L`, socat) makes its traffic look direct, hence exempt.
- Cross-origin `Origin` headers are refused on WebSocket upgrades and on every non-GET request.
- Connection bounds: a request head must arrive whole within 10 s (also the keep-alive idle limit); at most a quarter of the descriptor limit (16-1024) connections are served at once (an upgraded WebSocket no longer counts); `serve` raises its soft `RLIMIT_NOFILE` to the hard limit.

## Phone

Built for driving Claude Code and Codex from a phone (tested at 390x844 and 430x932 under iPhone 14 emulation):

- **Viewport**: the app is pinned to the visual viewport, so the on-screen keyboard shrinks the terminal instead of covering it (no page scroll or zoom on focus: fields are 16 px on touch screens); safe-area insets respected.
- **Row floor** (phones only): the PTY size is shared, so a phone never sizes it below 16 rows; a shorter terminal box (keyboard up, Compose open) is clipped around the cursor. Desktop panes fit their box exactly. With the keyboard up the quick answers shrink to one row and Compose hides the rest.
- **Needs you**: blocked and done agents of every workspace head the sidebar (the phone's switcher: drawer button, badge counts them) and are the phone's start screen (a phone opened without `#session=` lands there, never on a saved layout's pane); blocked first, longest-waiting first; worktree sessions show their branch (so do their browser notifications). Tap to open. Below 760 px the sidebar is a drawer; tablets get a narrower one.
- **Key bar** (touch screens; palette "Key bar: on/off/auto"), two rows. First, always in view: `esc` `tab` `shift-tab` (Claude's permission-mode cycle), sticky `ctrl` (tap: next key, twice: locked; applied to what the keyboard types), arrows (repeat when held; honour app cursor mode), `enter`, `^c`. Second, scrolls: sticky `alt`, `/ | ~ -`, `pgup` `pgdn`, text size; then `kbd` (show/hide the keyboard), `compose`.
- **Quick answers**: while the attached agent is blocked, the options of the prompt on the live screen (never the scrollback in view) become buttons: numbered lists (Claude's permission dialogs: sends the digit; an option ending `(y)` sends that key) or a `(y/n)` line (sends the letter + Enter); `esc` cancels. The status detail heads it.
- **Compose**: a multi-line text area (autocorrect, dictation) sent as one bracketed paste + Enter (`POST /paste` with `run`); `Paste` sends without Enter; history per device (arrows, or Up at the start); draft kept per session (switching sessions closes the panel; drafts of removed sessions are dropped); `Ctrl Enter` sends. Sign-out deletes history and drafts from the device.
- **Touch**: swipe scrolls the scrollback with momentum (in full-screen programs: wheel reports, else arrow keys); long-press selects a word, drag extends, then Copy / Copy screen; pinch or `A-` `A+` sets the text size (per device).
- **Alerts** (sidebar chip, per device): a system notification when an agent becomes blocked or done while the page is hidden, unfocused, or idle 5 min; each spell once, one per session and status per 15 s (a later spell waits for the window); clicking it opens that session. Needs https or localhost; on iOS only from the Home Screen app.
- **Install**: `/manifest.webmanifest` (standalone, flywheel icons) + `/sw.js`. The service worker never touches `/api`, sockets, or sign-in; it serves the cached signed-in page and `/assets/` only when the hub is unreachable (the page then says so and reconnects), is replaced with each hub build, and is emptied on sign-out. The Home Screen app has its own cookies on iOS: sign in once more there (`fw hub url --qr`).

## Flywheel panel

**Postgres-backed repos read the DSN from the hub's own environment** (`FLYWHEEL_PG_DSN`, else `DATABASE_URL`; flywheel.toml never carries one): start `fw hub serve` from a shell that exports it. One DSN serves every postgres folder; the store label (`postgres host:port/db`, no credentials) shows which database a panel read.

Workspace folders that are flywheel-adopted repos (a `flywheel.toml` or a `.flywheel/` dir) get a Flywheel panel: the verified loop beside the terminals. Read-only; `--no-flywheel` turns it off.

- **What it shows**: active phases (`tasks/active/<phase>/`) and their tasks as `pending | running | verified | failed | parked | awaiting | blocked` (from `fw status --json`: `verified` = a DONE lifecycle, `parked` = DONE but its landing parked); in-flight runs (the console's live rows); the 12 newest runs with their latest attempt's grader receipts; worker liveness (a live lease in `task_claims` -- an idle worker holds none) and autopilot liveness (`.flywheel/logs/autopilot/activity.json`); claims; and alerts (failed, stranded, stopped, awaiting a gate, blocked), which also join the sidebar's **Needs you** list: one row per alert while a folder has 1-2, else one row for the folder. **Mark seen** in the panel drops a folder's current alerts from Needs you in this browser until they change or new ones appear. Runs show DONE as `verified`.
- **Worktrees**: flywheel keeps a store per checkout (`fw worker` / `fw status` resolve `.flywheel/` under the worktree itself), so a workspace folder that is a linked git worktree of an adopted repo reads its own store (empty until a worker runs there) and its panel says `Linked worktree of <main>`; Start worker here runs there, against that store. A sibling-worktree session not in a watched folder gets its base folder's panel, labelled as that checkout's loop.
- **Where**: an adopted folder's sidebar chip gets a `verified/total` badge (plus the unseen alert count, red, pulsing while runs or daemons are live); click it, the `flywheel` header pill (attached session in an adopted folder; `Alt F`), a Needs-you row, or the palette.
- **Actions**: **Start worker here** opens a hub shell session `fw worker` (`fw worker 2`, ... when taken) running `<this fw> worker` in the folder, in a split below the focused pane (never over it; a new tab without room) (a real terminal; when it exits, Ctrl-C included, the shell stays; a restart restores it as a plain shell). While a worker the hub started still runs there, a start is refused (409) unless confirmed as another. **Output** on a task / run opens its `fw show` summary and its redacted agent transcript (`<store dir>/logs/runs/<run>.jsonl`, newest 1500 entries, followed live while the run is in flight).
- **Cost**: one watcher thread per adopted folder; every 2s it stats the policy, store (+ `-wal`), task tree, and activity record, and re-reads only on a change or every 15s (leases lapse with time). Each folder's own `flywheel.toml` is read (paths anchored at the folder, never the hub's cwd); stores are opened once and kept until the folder leaves every workspace (a failed read reconnects). A missing SQLite store is never created (tasks list as pending). Tracker sources (`github*`) are not listed (a network call per read): runs, claims, and alerts still show. Opening a store runs its idempotent schema bootstrap, as every `fw` read verb does.
- **Seam**: `flywheel-hub` defines `flywheel::FlywheelProvider` and plain data; the `flywheel` crate implements it (`hub_flywheel.rs`) over its console/status code and injects it into `fw hub serve`, so the hub never depends on the loop's crates.

## API

All under `/api`, JSON, token required. Errors are `{"error":"..."}` (unknown endpoints 404 and wrong methods 405 too); a malformed body is 400, a JSON body of the wrong shape 422. `cols`/`rows` of 0 mean the default (120x32).

| Method | Path | |
|---|---|---|
| GET / POST | `/workspaces` | list / create `{"name","folders":[...]}` |
| GET / PATCH / DELETE | `/workspaces/{id}` | PATCH takes `name` and/or `folders` (409 if it drops a running session's folder); DELETE refuses (409) while sessions run |
| GET / PUT | `/workspaces/{id}/layout` | `{"workspace_id","rev","tabs":[{"id","name","root"}]}`; node `{"type":"pane","id","session"\|null}` or `{"type":"split","dir":"right\|down","ratio","a","b"}`. PUT `{"rev","tabs"}` written against `rev` (0: never saved) -> the saved layout, `rev`+1; 409 `{"error","layout"}` when stale; 400 bad ids (`[A-Za-z0-9_-]`, 1-32, unique), names (1-32 chars, unique per workspace), ratio outside 0.05..0.95, another workspace's session, a session twice in one tab, over 12 tabs / 12 panes |
| POST | `/workspaces/import` | `{"path","name"?,"update"?}` (a `.code-workspace` on the hub's machine) -> `{"workspace","created","file","skipped":[{"entry","path"?,"reason"}]}`; 201 created, 200 updated, 409 name taken without `update` |
| GET | `/code-workspaces[?root=]` | `{"root","depth","files":[{"path","name","folders","paths","skipped","workspace_id"}\|{"path","name","error"}],"truncated"}` under `root` (default `--import-root`) |
| GET / POST | `/sessions[?workspace_id=]` | list / create `{"workspace_id","folder"?,"name"?,"kind"?,"command"?:[argv],"cols"?,"rows"?,"worktree"?:{"task","base"?}}` (`repo` = `folder`; `kind` defaults to `shell`; `command` only for shells, default `$SHELL`; default folder the first) |
| GET / PATCH / DELETE | `/sessions/{id}` | PATCH `{"name"}` renames; DELETE kills and forgets (204); `?remove_worktree=true[&force=true]` also removes companions + worktree: 200 `{"removed":[ids],"worktree":{"path","branch","branch_deleted"}}`, 409 with why when refused |
| GET | `/sessions/{id}/worktree` | `{"worktree","path","branch","base","missing","dirty","untracked","unmerged","refusal","companions","blocking"}` (runs git) |
| POST | `/sessions/{id}/companion` | `{"cols"?,"rows"?,"text"?,"run"?}`: the agent's companion shell (201 started now, 200 running); with `text` also pastes (as `/paste`) once ready: `{"session","created","ready":"running\|bracketed\|quiet\|timeout","paste"}` |
| GET | `/worktrees/plan?folder=&task=&base=` | `{"dir","default_base","branches","plan":{"path","branch","base"}\|null,"error"}` (the form checks as you type) |
| POST | `/sessions/{id}/kill` | 202: SIGHUP the session (SIGKILL after 3s); record kept. 409 when nothing is left to kill |
| POST | `/sessions/{id}/relaunch` | relaunch an ended (`exited`/`stopped`) session in place: same id, name, command, folder; fresh PTY + scrollback. Body `{"cols"?,"rows"?}`. 409 while it runs. `/restart` is its old name |
| GET | `/sessions/{id}/screen[?source=visible\|recent&lines=N]` | rendered plain text (below) |
| POST | `/sessions/{id}/input` | `{"data","paste"?,"enter"?}` -> `{"bytes","bracketed"}`: type into it (`paste`: as `fw hub send`). 409 when it has no running terminal or is not reading |
| GET | `/sessions/{id}/commands[?limit=N&all=true]` | suggested code blocks, newest first (shell only unless `all`; `limit` 1..500, default 50): `{"session_id","conversation","transcript","commands":[{"seq","conversation","message_id","timestamp","block_index","lang","shell","text","command","source"}]}` |
| POST | `/sessions/{id}/paste` | `{"text","run"?}` -> `{"bracketed","ran","bytes"}`: paste a suggested command (as `/input` with `paste`, plus: trailing newlines dropped, at most 1 MiB, whitespace-only is a 400); 409 when not running, not reading, or unsafe multi-line |
| GET | `/sessions/{id}/ws` | WebSocket attach |
| GET | `/events[?client=ID]` | hub-wide events WebSocket (below) |
| GET | `/kinds` | `{"kinds":[{"kind","available","program"}]}`: which session kinds this host can launch |
| POST | `/login-codes` | `{"code","expires_in":300}`: a one-time code for `/login?code=` |
| POST | `/token/rotate` | 204: rotate the token (see `fw hub token --rotate`) |
| GET | `/folders/check?path=` | always 200 (even without `path`): `{"ok":true,"path":canonical,"git"}` or `{"ok":false,"error"}` (the workspace form validates as you type); `/repos/check` is the old name |
| GET | `/notify` | `{"push":null}` or `{"push":{"host","events","debounce_secs","settle_secs","detail"}}` |
| POST | `/notify/test` | 204 sent; 409 push off; 502 `{"error"}` the webhook refused or was unreachable |
| GET | `/flywheel` | `{"enabled","folders":[summary]}`; summary `{"folder","generated_at","phases","counts":{state:n},"alerts":[{"kind","subject","detail","run_id"}],"live_runs","worker_live","autopilot_live","problem","worktree_of"}` (`worktree_of`: the main checkout when the folder is a linked worktree, else null) |
| GET | `/flywheel/panel?folder=` | the whole panel (`phases`, `live`, `runs`, `worker`, `autopilot`, `claims`, `alerts`, `source`, `store`, `worktree_of`, `problem`); 404 unless an adopted workspace folder |
| GET | `/flywheel/run?folder=&run=&since=N` | `{"run_id","task_id","status","active","summary","from","entries":[{"ts","kind","header","body","error"}],"next"}`; poll with `since=next`. 400 bad run id, 404 unknown run, 503 while an earlier read of it still waits on the store |
| POST | `/flywheel/worker` | `{"folder","workspace_id"?,"another"?,"cols"?,"rows"?}` -> 201 the new `fw worker` session; 409 `{"error","running":{"id","name"}}` while a hub-started worker runs there and `another` is not true |

WebSocket: server sends **text** `{"type":"snapshot","cols","rows","alt_screen","bytes"}`, then **binary** frames (the first `bytes` bytes are the snapshot, then live output), and **text** `{"type":"exit","code","signal"}` / `{"type":"reset"}` (fell behind: clear, a screen-only snapshot follows) / `{"type":"size","cols","rows"}` (a viewer resized it; in order with the output) / `{"type":"error","message"}`. Clients reset, size their terminal to `cols`x`rows`, replay the snapshot, then fit and send `resize` (the snapshot starts with RIS anyway). Client sends `{"type":"input","data":"..."}`, `{"type":"resize","cols":N,"rows":N}`, `{"type":"theme","foreground","background","cursor":"#rrggbb","ansi":[16 x "#rrggbb"]}` (the colours colour queries answer with), or binary raw input. Token rotation closes every socket with code 4001 (even one whose upgrade raced the rotation); the UI then shows sign-in with a "token was rotated" notice.

Session views carry `agent` (`{"status","detail"?,"since"}`, agent kinds only), `agent_session_id`, `worktree`, `companion_of`, and `git` (`{"branch","dirty","untracked","ahead","behind","against"?}` from the badge cache; absent until read or outside git).

Events WebSocket (`client`: 1-64 of `[A-Za-z0-9_-]`, else 400; omitted, the connection gets its own): server sends `{"type":"snapshot","sessions":{id:status},"workspaces":{id:rollup}}` on connect and after falling behind, `{"type":"status","session_id","status"|null,"workspace_id","workspace_status"}` per change (as this client sees it), and `{"type":"sessions"}` when sessions or workspaces changed (re-list), and `{"type":"commands","session_id","fresh","commands":[...]}` when an agent suggested code blocks (newest first; `fresh`: written in the last 2 minutes, not a re-read backlog), `{"type":"flywheel","folder","summary"|null}` when an adopted folder's panel changed (`null`: no longer watched), and `{"type":"flywheel_reset"}` after missing some (re-list `/api/flywheel`), `{"type":"layout","workspace_id","rev"}` when a layout was saved or pruned (re-read it). `{"type":"sessions"}` also follows a terminal socket attaching or leaving (`live.clients`). Client sends `{"type":"view","session_id":id|null,"visible"?:[ids]}` (the focused pane's session and the other panes on screen; all count as viewed; nothing while hidden). Token rotation closes it with 4001 too.

Screen text: `source=visible` (default) is the screen's rows (`lines` is refused); `recent` the last `lines` (1..100000, default 100) logical lines of scrollback + screen, soft wraps joined, trailing blank rows dropped. Both read the active screen.

```json
{"source":"visible","cols":80,"rows":24,"alt_screen":true,"title":"vim","cursor":{"row":2,"col":2,"visible":true},
 "modes":["app_cursor","bracketed_paste","mouse_drag","sgr_mouse"],"lines":["..."]}
```

```bash
T=$(fw hub token)
curl -s -H "Authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"name":"flywheel","folders":["/home/me/code/flywheel"]}' http://127.0.0.1:7420/api/workspaces
```

## Web UI

Sidebar: workspaces (collapsible, with an agent rollup chip: blocked / working / done) > sessions, each with its kind glyph and a status dot (live / done / failed / stopped / gone; ended sessions keep their exit code across hub restarts; a running agent shows working / blocked / done / idle instead, blocked with an alert edge). The tab title and favicon badges count blocked and done agents (`(1 blocked, 2 done) ...`; one favicon badge each); on narrow screens the drawer button badges agents in other sessions that need you. A blocked row shows what it asks under its name. Status and list changes are pushed over the events socket; the UI polls only while that socket is down. Dialogs create workspaces (folder paths checked live, plain folders marked "no git", optional per-folder labels; paste several lines at once; or **Import VS Code workspace**: the `*.code-workspace` files under a search root you can change, Import / Update per file (Update first shows what it adds and drops), or a pasted path, then a report of every folder and every skipped entry, duplicates included) and sessions (shell | claude | codex, folder picker defaulting to the attached session's folder, with a filter past 5 folders; missing folders disabled; on a git folder, an optional new sibling worktree with its path, branch, and base previewed live). Rows in git directories carry a branch line (`wt` for worktrees, ↑ahead ↓behind ~changed ?untracked); companion shells hang under their agent. Sidebar folder chips: filled dot = git, hollow = plain folder, struck red = missing. The attached session is kept in the URL (`#session=<id>`), so reloads re-attach; a dropped socket keeps the last screen, shows a countdown banner (and how many keystrokes were not sent), and reconnects with backoff (immediately on `online` / tab focus). An ended process, or a session whose terminal did not survive a hub restart, offers Relaunch (in place) and Close. A viewed session removed elsewhere (another browser, `fw hub kill --rm`, its workspace deleted) leaves its pane (a toast says so while other panes remain; with none left, the empty stage says "Session removed." and why) -- nothing to relaunch; so does a stale `#session=` link on load. Only a session list fetched after the attach can declare it gone (overlapping refreshes are sequenced). Themes: auto (follows the OS), dark, light; the light theme lifts low-contrast agent TUI colours to 4.5:1.

| Key | |
|---|---|
| `Ctrl K` | command palette (fuzzy: sessions, actions, themes); again inside it sends a literal `^K` |
| `Alt N` / `Alt Shift N` | new session / new workspace |
| `Alt 1`..`9`, `Alt Up/Down` | jump to the nth / previous / next session with a terminal (numbers skip gone ones) |
| `Alt R`; `F2` or double-click on a sidebar row | rename session |
| `Alt W`; `Delete` on a focused sidebar row | close session (confirms while it runs) |
| `Alt C` / `Alt Shift C` | Commands drawer / copy the last suggested shell command |
| ``Alt ` `` | flip between an agent and its companion shell (opened on first use) |
| `Alt F` | Flywheel panel of the attached session's folder |

The **Commands drawer** (`suggested` in the header, `Alt C`, or the palette; it overlays the terminal so opening it never resizes the PTY for other viewers) lists the attached agent's code blocks newest first (from a shell session: the workspace's agent that suggested last, or the one picked), unwrapped in monospace, each with Copy (the clipboard API on https/localhost, a selected-textarea fallback on plain-http LAN and iOS Safari), Send (paste into the chosen running shell of the workspace, no Enter) and Send + Run. "send to" defaults to the agent's companion shell, else the attached shell, else a shell in the agent's directory; with none, it is "new shell (companion)" and the buttons read Open shell & send / Open shell & run (start the companion, wait for its prompt, paste). A line under the pickers says where Send goes. Keys in the drawer: arrows / `j` `k` pick, `Enter` or `c` copy, `1`-`9` copy the nth, `s` Send, `Shift S` Send + Run, `Esc` close. Lines wider than the card fade at the edge with a `wide: scroll` hint. A new shell command from an agent in the workspace in view raises a toast with Copy. The header badge counts the drawer's source only: the attached agent's own unseen blocks (from a shell, the agent that suggested last, named beside the count).

Hub shortcuts are captured before the terminal sees them. On macOS the chord is `Ctrl Option` instead of `Alt`, so Option keeps typing layout characters (`| [ ] { }`, dead keys); elsewhere an `Alt` key the layout turns into a character also goes to the terminal. Served from the binary (no build step): xterm.js 6.0.0 + fit + web-links and the UI fonts (Big Shoulders Display, Martian Mono, JetBrains Mono; SIL OFL), vendored under `crates/flywheel-hub/web/vendor/` with their licenses. Only the fonts, icons, `login.js`, `/manifest.webmanifest`, and `/sw.js` are served without the token (the login page and installers use them; none holds app data).

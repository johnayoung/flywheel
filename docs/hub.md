# Hub

`fw hub` is a web workspace for AI coding agents. The hub server owns every terminal: each session is a real PTY (shell, prompts, running processes, scrollback) that browsers attach to and detach from without disturbing it. Many clients can watch one session. Crate: `crates/flywheel-hub`.

**Security first:** a hub serves shells over HTTP -- anyone with the token, a peer credential for it, or a shell in one of its sessions has a shell as you. An agent session runs as you and can read the state dir: the token, and `machines.json`'s peer credentials for every registered machine (so a hijacked agent reaches them too). It binds `127.0.0.1` by default; for remote access put it behind `tailscale serve`, or use `--bind 0.0.0.0 --tls auto` (built-in https, see HTTPS without Tailscale) -- never plain `--bind 0.0.0.0` on an untrusted network.

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
fw hub tls ca [--out FILE [--der]] | rotate   # built-in https: the local CA's fingerprint + constraints / replace it; see HTTPS without Tailscale
fw hub commands SESSION [--last] [--all] [--json] [--copy] [--limit N]   # agent-suggested code blocks
fw hub search QUERY [--regex] [--case] [--session S] [--workspace W] [--kind K]... [--source scrollback|transcript|commands]... [--limit N] [--per-session N] [--offset N] [--json]   # see Search; exit 0 hits, 1 none
fw hub report --source claude|codex --event E [--detail D] [--stdin] [PAYLOAD]   # agent hooks only
# start/serve also take --handoff (take over from the running hub; --bind/--port ignored)
# SERVE FLAGS (each also a hub.toml key; a flag wins): [--bind ADDR] [--port N] [--public-url URL]... [--allow-origin ORIGIN]... [--trusted-proxy IP]... [--tls auto|off|CERT,KEY] [--tls-name NAME]... [--scrollback-lines N] [--no-restore] [--no-agent-resume] [--no-flywheel] [--import-root DIR] [--notify-url URL] [--worktree-branch-prefix P] [--record-part-cap SIZE] [--record-dir-cap SIZE] [--share-default-expiry D] [--share-max-expiry D]
fw hub machine add NAME URL [--mint [--peer-name N]] [--fingerprint SHA256] [--replace]   # register another hub (secret from FW_HUB_MACHINE_SECRET or stdin); --replace re-pairs NAME in place; see Machines
fw hub machine ls [--no-probe] | rm NAME | test [NAME]     # [--json]
fw hub peer issue --name X | ls | revoke NAME|ID           # credentials other hubs hold to reach this one
fw hub config check [--file PATH] | show [--json]          # <state-dir>/hub.toml: validate / settings in force + source; see Settings
fw hub plugin ls [--json] | enable NAME | disable NAME | logs NAME [-n N] [--json]   # local plugins; see Plugins
fw hub share SESSION [--expires D] [--name LABEL] [--qr] [--json] | share ls | share revoke ID|LABEL|--all [--session S]   # read-only live view links; see Share links
fw hub record start|stop SESSION | ls | get NAME [-o FILE|-] [--force] | rm NAME   # asciinema v2 recordings; see Recording
fw hub doctor [--port N] [--json]                          # check this machine for the hub, each finding with a fix; exit 1 if any fails; see Doctor
# every verb but report (hooks only) takes --state-dir DIR
# ls/new/send/read/wait/attach/kill/relaunch/commands/search/template/explain/review/merge/open/fanout/share/record/url also take [--url URL] [--token T]: a hub elsewhere
#   and (all but url, share, record) [--machine NAME]: a machine registered on that hub, through its proxy (`fw hub ls --machine desktop`)
#   (token: prefer FW_HUB_TOKEN; without one, the state dir's token goes only to that state dir's running hub)
#   https --url: trusted by the system roots plus this state dir's local CA, or [--fingerprint SHA256] (the hub's `fw hub tls ca`)
```

Scripting the running hub, or the one `--url` names (sessions/workspaces by id, unique id prefix, or name; exact name beats prefix; ambiguity is an error):

```bash
fw hub ls [--json]                                         # workspaces > sessions: short id, kind, state (+ agent status), name, live cwd [branch +ahead -behind ~changed ?untracked]
fw hub new WS [--kind shell|claude|codex] [--folder PATH|LABEL] [--name N] [--create] [--worktree TASK [--base REF]] [--json] [-- ARGV...]   # prints the id; a taken --name is refused (--repo = --folder)
fw hub new WS --kind claude|codex [--prompt TEXT|-] [--model M] [--permission-mode P] [--approval A] [--sandbox S] [--template NAME]   # agent launch; see Agent launcher
fw hub template ls [WS] [--json] | save WS NAME --kind claude|codex [launch flags] [--worktree] [--replace] | save WS NAME --edit [flags] [--no-worktree] | rm WS NAME   # per-workspace launch templates
fw hub send SESSION TEXT|- [--enter] [--raw]               # pasted: newlines -> CR, bracketed when the program enabled it
fw hub read SESSION [--source visible|recent] [--lines N] [--json]
fw hub explain SESSION [--json]                            # why its status: hook | manifest:<file>#<rule> | heuristic, + screen rule, evidence rows, rules tried
fw hub explain --file SCREEN.txt --kind claude|codex|shell [--json]   # a saved screen (`read > f`, `-` = stdin) vs this machine's manifests; no hub
fw hub wait SESSION --until done|blocked|idle|working|exit [--timeout SECS] [--changed]   # exit 0 met, 124 timeout, 1 error
fw hub attach [SESSION] [--prefix C-b] [--no-status]      # this terminal on a session (picker without SESSION); see Terminal client
fw hub kill SESSION [--rm [--remove-worktree [--force]]]   # hang up (record kept, as exited); --rm also forgets it; see Worktrees
fw hub relaunch SESSION                                    # start an exited/stopped session again in place
fw hub import FILE.code-workspace... [--name N] [--update] [--json]   # VS Code multi-root workspaces -> workspaces
fw hub review SESSION [--base REF] [--json] [--diff PATH [--scope committed|staged|unstaged|untracked]]   # see Review
fw hub merge SESSION [--yes] [--rebase] [--remove-worktree] [--json]   # ff the base onto a worktree branch in the main checkout
fw hub merge SESSION --abort-rebase [--yes] [--json]                     # abort the worktree's stopped rebase
fw hub handoff FROM TO (--selection-file F|- | --diff [--path P] [--base REF] | --last-command | --command SEQ | --message N) [--to-machine M] [--max-bytes N] [--edit] [--dry-run] [--json]   # see Handoff
fw hub ask-review SESSION [--kind claude|codex] [--base REF] [--dry-run] | --findings | --send-findings [--json]   # see Handoff
fw hub fanout new WS (--prompt TEXT|- --grader [name=]CMD... | --from-task ID [--prompt ..] [--grader ..]) --agent KIND[:MODEL]... [--folder F] [--base BRANCH] [--name N] [--timeout SECS] [--protect PATH]... [--permission-mode M] [--approval A] [--sandbox S] [--no-commit-instruction] [--json]   # see Verified fan-out
fw hub fanout ls | show FANOUT [-v] | wait FANOUT [--timeout SECS] | grade FANOUT CAND | fix FANOUT CAND [--yes] | pick FANOUT CAND [--yes] [--rebase] | cleanup FANOUT [CAND...] [--stop-only] [--force] [--yes] | rm FANOUT   # [--json]
fw hub fanout promote FANOUT [--task-id ID] [--phase NAME] [--goal TEXT] [--reference CAND|none] [--yes] [--json]   # see Fan-outs and flywheel tasks
```

`import` reads VS Code workspace files (JSONC: comments, trailing commas): each `folders` entry's `path` (relative to the file's directory, absolute, or `~/`) or `file://` `uri` (and, on WSL, `vscode-remote://wsl+<this distro>/...`) becomes a folder, its `name` the folder's label. The workspace is named after the file (`personal.code-workspace` -> `personal`) unless `--name` (one file only). Missing folders and entries naming no local directory (other remotes, Windows paths) are listed as skipped (stderr summarizes); a file with nothing importable fails. An existing workspace of that name is refused unless `--update`, which replaces its folders and labels (refused when that drops a folder a running session is in). Every file is tried; exit 1 if any failed.

`open` mints a one-time code for `http://<loopback>:<port>/login?code=...[#session=<id>]` (under `--url`: that URL's `/login?code=...`; `url --url` prints the same), prints that URL, and writes a 0600 redirect page (`open-<id>.html` in the state dir; expired ones are swept) that forwards to it -- the code never goes on an opener's argv (`/proc/*/cmdline` is world-readable). The page goes to `$BROWSER` (`:`-separated commands, `%s` = its `file://` URL) when set, else on WSL `wslview`, else `cmd.exe /c start` (its `\\wsl.localhost\...` path, `^`-escaped), else `xdg-open` (macOS: `open`). Only an opener that cannot start falls through to the next; one that runs and fails ends it (exit 1: it may have opened a tab, and the code is single-use).

`new --create` makes workspace `WS` (with `--folder`) when none matches; `--folder` is a path (relative to the caller's directory, and on the hub's machine under `--url`) or one of the workspace's folder labels or directory names (`--folder john-finances`). `send` wraps text in `ESC[200~ ... ESC[201~` only when the program set bracketed paste (markers inside the text are stripped, repeatedly, so split markers cannot reassemble), a final newline becomes that Enter (`printf 'make\n' | fw hub send s -` runs it), and sends Enter 50 ms later as its own keypress; `--raw` sends bytes untouched (`fw hub send s $'\x03' --raw` is Ctrl-C).

## Doctor

`fw hub doctor` checks what a hub here depends on and prints `ok | info | warn | FAIL` per finding, each with a `fix:`. Read-only: nothing is created, removed, rewritten, or locked (the port is bound for a moment; programs are asked `--version`/`--help`; scripts found on PATH are read, never run; the serve lock is seen in the recorded hub process's `/proc/<pid>/fdinfo`, never taken).

- `fw` / `flywheel` on PATH: the first match must be this binary (same file, a byte-identical copy, or its `fw`/`flywheel` sibling from the same install: same directory, written within 10 minutes, same size within 1%). A script there (e.g. a Python venv's `fw` entry point ahead of `~/.cargo/bin`) is FAIL with its interpreter named; another flywheel build is warn, with its `fw --version` against this one's (a build without `--version` is named older); shadowed matches are listed.
- `hub`: the state dir's running hub (pid, version, binary; never trusts a stale `server.json`); a version other than this CLI's suggests `fw hub upgrade`.
- `claude` / `codex`: found and `--version` answers; also resolved on the running hub's own PATH (`/proc/<pid>/environ`). `claude hooks`: the hub's hook JSON has Claude Code's hooks shape, `claude --settings <it> --version` takes it (claude only parses the JSON there; it checks hook entries when a session starts, so a Claude session turning `working` on its first prompt is the full proof), and `fw hub report` runs.
- `state dir` (yours, mode 700, writable) and secrets (`token`, `logins.json`, `notify.toml`, `machines.json`, `peers.json`, `shares.json`, and the files in `recordings/` and `plugin-logs/` not group/other readable).
- `hub.toml` (parsed and resolved by the same validation as `fw hub config check`, `[recordings]` and `[shares]` included), `notify.toml`, `settings.json`, plugin manifests.
- `recordings`: what `<state-dir>/recordings/*.cast` take against `[recordings] dir_cap` (info at 90%, warn past it: the next start or rotation deletes the oldest finished ones).
- `port` (free, or this state dir's hub; one taken by another flywheel hub is named as such; a taken one suggests a free one), `tailscale` (installed, connected, `tailscale serve` for the port), `browser` (what `fw hub open` would run), `systemd --user` + `linger` (for `install-service`; WSL: `/etc/wsl.conf` `systemd=true`), `clock` (NTP sync; WSL drift fix), `disk` (free space at the state dir: warn < 2 GiB, fail < 512 MiB; log size).

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

`send` types, `wait` blocks on the events socket (no polling), `read` / `commands` read back. `wait --until idle` accepts `done`; `--changed` ignores the state current at start (the previous turn's `done`), but against this machine's hub counts any status that began after the wait process started (Linux: to the 10 ms tick), so `fw hub wait --changed &` then a prompt cannot miss a fast turn; an agent wait fails (1) if the session ends first; `--until exit` prints `exited N` / `killed (SIG)`. `--timeout` bounds everything, connecting included, but the state is always checked once (`--timeout 0`: check and return; reaching the hub may take up to 1 s).

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

Replaces the running hub process with a new one **without ending any session**: same processes, pids, screens, scrollback, port, agent status/seen state, and hook secrets. Use it after every rebuild instead of stop/start. Linux; exit 0 handed off, 4 refused (nothing changed), 3 no hub, 1 failed.

- Binary: `--binary PATH`, else `fw` on `PATH`, else this program. It is probed first (`<binary> hub start --help`): not a flywheel with `fw hub` -> error, nothing asked of the hub (an unusable `fw` on `PATH` falls back to this program with a note). The new hub gets the **old hub's environment** (`/proc/<pid>/environ`), so sessions created after the upgrade resolve `claude`/`codex`/shells as before. Agents spawned before keep their status hooks pointing at the old binary's path: upgrade in place (`cargo install`) or keep it until they are relaunched.
- Mechanism: the hub listens on `<state-dir>/handoff.sock` (0600, same-user peers only). `upgrade` asks it for its serve flags and runs `<binary> hub start --handoff <flags>`, i.e. `serve --handoff` detached. The old hub freezes (stops accepting -- connections wait in the listen backlog, so the port never closes; requests on open connections get 503; PTYs paused; reports paused; transcript offsets saved), closes every WebSocket with **1012** (service restart), passes the listening socket, serve lock, report socket, and per session the PTY master + pidfd + full emulator snapshot + held input over `SCM_RIGHTS`, and exits only after the new hub acks `started`. Input that reaches the old hub after its sessions were handed over is refused (`input dropped: the hub is handing over`; HTTP 503), never silently lost.
- Clients: browsers reconnect at once on 1012 (focus kept, "Hub upgraded" toast with the version); `fw hub attach` and `fw hub wait` reconnect too (up to 2 min). `fw hub status` shows the serving version and binary.
- Failure: anything before the commit (refusal, error, new hub dying) thaws the old hub, which keeps serving; resizes sent during the freeze are dropped. The old hub stays frozen at most `--timeout` per step waiting for a silent new hub, then rolls back; `upgrade` then kills the stuck new hub (only once the old hub's log shows the rollback) and says so. A new hub that dies after it started resuming sessions: the old one rolls back without replaying the held input (never typed twice). The old hub vanishing after the new one is ready -> the new one takes over.
- Refusals: version skew (`handoff::PROTOCOL` mismatch), a hub predating handoff (no `handoff.sock`), or a binary without `--handoff` -> refused; `--allow-restart` then stops and starts it (restore path: processes restarted) with the old hub's serve flags (its command line, the port it actually listens on; else `server.json`: address and public URLs) and environment; a start that fails brings the old binary back. Another handoff under way, requests or sessions that would not settle -> refused, retry (never a restart). A systemd unit's hub refuses, and is never stopped/started by `upgrade` (that would take it out of its unit): `systemctl --user restart <unit>`.
- Handed-over processes are no longer the hub's children: their exit is seen via the pidfd (Linux 5.3+; else polling the pid) and recorded with **unknown exit code** (`exited ?` in `ls`, "exit code unknown" in the UI). The new hub runs no restore pass (stopped sessions wait for the next normal start). macOS: compiles, polls pids; untested.

`serve` runs in the foreground and prints one-time login URLs (below); when any is reachable from another device (non-loopback bind, `--public-url`, or a detected `tailscale serve`) it also draws a QR code of the first (only when stdout is a terminal; logs get a pointer to `url --qr`). One hub per state dir: the hub holds an exclusive lock on `<state-dir>/serve.lock` for its whole life (guarding `server.json` and `report.sock`), and `serve` refuses to start while another holds it. Ctrl-C / SIGTERM records every running session stopped, then hangs them up (SIGHUP, then SIGKILL to anything left in each session, `nohup`ed children too, before the process exits). While it runs, `<state-dir>/server.json` (0600) records its pid, address, and public URLs for `url` / `token --rotate`; one left behind by a crashed hub (lock not held) is ignored and removed, so the CLI never sends the token to whatever now listens there.

`url` asks the running hub for a fresh one-time code and lists it under every URL that reaches the hub, most remote-friendly first: `public` (`--public-url`), `tailnet https (tailscale serve)`, `tailnet (MagicDNS)` / `tailnet` (100.x, only when an interface carries it: userspace-networking tailscale cannot reach a bind), `lan (<iface>)` (non-loopback binds; container bridges skipped), `this machine`. `--qr` draws the first remote one (black-on-white half blocks, any terminal theme). The long-lived token is never printed by `serve` or `url`.

`token --rotate` replaces the token. A running hub switches at once (no restart): every browser cookie, outstanding code, and open terminal or events socket (closed with code 4001; the UI returns to sign-in) of the old token stops working; bearer scripts need `fw hub token` again. Agent status hooks are unaffected (they authenticate with per-spawn secrets, not the token). With no hub running it just rewrites the file.

`report` is what agent sessions' hooks run (see Agent status); it needs the session's environment, never prints to stdout, and fails with exit 1 (never 2, which would stop a Claude `Stop`).

Every flag below but `--notify-url` and `--state-dir` is also a `hub.toml` key (see Settings); a flag given wins over the file.

| Flag | Default | |
|---|---|---|
| `--bind` | `127.0.0.1` | non-loopback prints a warning |
| `--port` | `7420` | `0` picks a free port |
| `--public-url` | none | origin browsers reach the hub at through a proxy (`https://box.tailnet.ts.net`); accepted as an origin, listed first by `url`, `Secure` cookies when https. Origin only (no path) |
| `--trusted-proxy` | none | proxy on another machine whose `X-Forwarded-*` are believed (loopback always is), so its clients are rate-limited per client, not as one address |
| `--allow-origin` | none | extra browser origin accepted without being listed; same-origin always allowed |
| `--notify-url` | none | ntfy-compatible push on blocked/done (below); overrides `url` in `<state-dir>/notify.toml` |
| `--scrollback-lines` | `5000` | per-session scrollback; capped at 1M cells (lines x cols), so wide terminals keep fewer. Memory: 24 B per cell, so a full 5000 x 120 session holds ~14 MiB (50 such sessions ~700 MiB); lower it for many sessions |
| `--worktree-branch-prefix` | none | session worktree branches are `<prefix><task>` (e.g. `agent/`) |
| `--import-root` | `~/code` | where the UI's VS Code import looks for `*.code-workspace` (3 levels deep; hidden, `node_modules`/`target`/... and symlinked dirs skipped) |
| `--record-part-cap` | `64MiB` | a recording part past it rotates (`64KiB`-`4GiB`; bytes or `KiB`/`MiB`/`GiB`) |
| `--record-dir-cap` | `1GiB` | recordings directory cap, oldest finished deleted first (at least the part cap, at most `1TiB`) |
| `--share-default-expiry` | `1h` | a share link that names no expiry (no longer than the max) |
| `--share-max-expiry` | `30d` | longest share link expiry accepted (`60s`-`30d`) |
| `--state-dir` | `$XDG_STATE_HOME/flywheel/hub`, else `~/.local/state/flywheel/hub` | |

## Settings and hub.toml

Two layers, both validated by the server:

- **`<state-dir>/hub.toml`** -- the hub's own settings (what `serve` flags set). Precedence per key: **flag > file > default** (a list flag replaces the file's list). Unknown keys and bad values are errors naming the key. Invalid at start: `serve` refuses (exit 2). Edited while running: re-read within 1 s; valid -> live keys apply at once (`config show` lists the rest as waiting for a restart); invalid -> the last good settings stay, the error goes to the log, `GET /api/config`, a UI toast, the gear (red), and `fw hub config check` (exit 1).
- **Settings** (palette "Settings", the sidebar's gear, `Alt ,`; a full-screen sheet on phones) -- UI preferences kept by the hub in `<state-dir>/settings.json` (0600, revisioned), so they follow you to every device; any one can be kept for **this device** only (browser local storage). Per-browser choices made before Settings existed (theme, pane prefix, text size, key bar, alerts) became that browser's device overrides.

```toml
[server]                          # at restart
bind = "127.0.0.1"
port = 7420
public_urls = ["https://box.tailnet.ts.net"]
allow_origins = []
trusted_proxies = []
tls = "off"                       # "auto" | "off" | "/abs/cert.pem,/abs/key.pem"
tls_names = []                    # extra DNS names, IPs, networks ("192.168.0.0/16") for the local CA
[sessions]
scrollback_lines = 5000           # live: sessions started from now on
restore = true                    # at restart (--no-restore)
agent_resume = true               # at restart (--no-agent-resume)
[worktrees]
branch_prefix = "agent/"          # live
[import]
root = "~/code"                   # live
[flywheel]
enabled = true                    # at restart (--no-flywheel)
[agents.claude]                   # live: fills a launch (CLI, API, web) that names none
model = "opus"
permission_mode = "acceptEdits"
[agents.codex]
model = "gpt-5-codex"
approval = "on-request"
sandbox = "workspace-write"
[plugins]
enabled = true                    # live: false runs no plugin
max_concurrent = 4                # at restart (1-16)
[recordings]                      # live: running recordings too (next event / next sweep)
part_cap = "64MiB"                # 64KiB-4GiB; bytes or KiB/MiB/GiB
dir_cap = "1GiB"                  # >= part_cap, <= 1TiB
[shares]                          # live: links made from now on
default_expiry = "1h"             # <= max_expiry
max_expiry = "30d"                # 60s-30d
[review]                          # live
reviewer = "other"                # ask-review's kind: other (claude <-> codex) | same | claude | codex
```

Not in hub.toml: push (`notify.toml`, 0600: it holds secrets), detection overrides (`agent-detection/`), plugins (`plugins/`). `fw hub config show` asks the running hub (else reads the file; flags unknown) and prints every key, value, source (`flag | file | default`), and when it applies.

| Settings section | |
|---|---|
| Appearance | theme, terminal font (JetBrains Mono / Martian Mono / system), text size (unset: 13, phones 11), density (compact) |
| Terminal | scrollback in view (client side; the hub's own is `sessions.scrollback_lines`), cursor style / blink, ligatures in transcripts, commands, run output, diffs (off) |
| Keyboard | pane prefix, key bar, companion key mode, shortcut tour (offered on the start screen until seen or dismissed); **every shortcut**, rebindable (below) |
| Notifications | system notifications (device only: a browser permission), command toasts |
| Agents | the new-session form's pre-filled model / permission mode / approvals / sandbox per kind |
| Plugins | each plugin: enable toggle, events, actions (run), last run, last error, logs |
| Server | read-only: effective hub.toml with sources, file error, pending restart |

**Keys**: each action has up to 4 bindings, `mod+...+key` (`mod` = Ctrl, Cmd on macOS; `alt` = Alt, Ctrl+Option on macOS; then `shift`, `meta`; key = the physical key: `a`-`z` `0`-`9` `f1`-`f24` arrows `pageup` ... `` ` - = [ ] \ ; ' , . / ``; `alt+1..9` for the nth-session action). Pane-prefix keys bind the character typed (`%`, `"`, `z`) or `ctrl+arrow`. Saved for all devices or "this device only" (kept in the browser per hub id, so another hub later on the same origin starts clean). The UI's key hints (sidebar chips, home page, titles) follow the keymap. The server refuses (422, every finding named): keys the browser/OS keeps (`ctrl+w`, `ctrl+t`, `ctrl+n`, `ctrl+tab`, `ctrl+pageup/down`, `ctrl+shift+i/j/c`, `ctrl+alt+delete`, `alt+f4`, `alt+tab`, `f11`, any `meta+`), keys terminals need (no modifier, `shift+` only, `ctrl+c` `ctrl+d` `ctrl+z` `ctrl+\` `ctrl+[` ...), a key bound twice in one scope (`mod+k` = `ctrl+k`), and a prefix action on the prefix key itself; it warns on keys a shell loses (`alt+b/f/d/.`, `ctrl+<letter>`, `ctrl+left/right`) and keys some desktops take (`ctrl+alt+t/l/arrows`, `ctrl+space` (IME), `ctrl+shift+v`). Rules in `GET /settings` `rules` (`reserved`, `terminal`, `shadowed`, `desktop`). Fixed: in-dialog navigation (arrows, `j` `k`, Enter, Esc), F2 / Delete on sidebar rows. The keys reference (`?` after the prefix, palette "Keyboard shortcuts") lists the bindings in force.

| Method | Path | |
|---|---|---|
| GET | `/settings` | `{"rev","values","keys","keymap","prefs":[spec],"actions":[{"id","group","label","scope","indexed","defaults"}],"rules":{"reserved","terminal","shadowed","desktop"},"load_error","file"}` |
| PUT | `/settings` | `{"rev","values"?:{id: value\|null},"keys"?:{action: [keys]\|null}}` -> the view + `warnings`; 409 stale `{"error","settings"}`; 422 `{"error","errors":[{"action","key","message"}]}` |
| POST | `/settings/keys/check` | `{"keys":{action:[keys]}}` -> `{"keys","errors","warnings"}` (saves nothing) |
| GET | `/config` | `{"file":{"path","exists","error","loaded_at"},"rev","values":[{"key","value","source","apply"}],"pending_restart","running"}` |

Events socket: `{"type":"settings","rev"}`, `{"type":"config","rev"}` (re-read).

## Plugins

Local executables the hub runs on its events and as palette actions (herdr's model). **They run as you, with your user's access, unsandboxed: the hub reviews nothing.** Nothing installs one but you: copy a directory to `<state-dir>/plugins/<name>/` (`[a-z0-9][a-z0-9_-]{0,31}`); the UI and API can enable, disable, and run plugins, never download one. A plugin found is **disabled** until `fw hub plugin enable NAME` (or its Settings toggle): `<state-dir>/plugins.json`. Sample: `crates/flywheel-hub/plugins/hello`.

```toml
# <state-dir>/plugins/ping/plugin.toml
description = "Ping me when an agent is stuck"
command = ["./hook.sh"]            # argv, no shell; ./x is the plugin dir's, a bare name is looked up on PATH
timeout_secs = 10                  # 1-300
[[events]]
on = "status.changed"              # hub.started session.created session.exited session.removed status.changed
statuses = ["blocked", "done"]     #   review.committed review.pushed review.pr_opened review.rebased review.merged fanout.finished
                                   #   share.created share.revoked record.started record.stopped
                                   #   review.requested handoff.sent
kinds = ["claude", "codex"]        # optional: sessions of these kinds
[[actions]]                        # palette "Plugin: <title>", Settings > Plugins
id = "summarize"
title = "Summarize this session"
context = "session"                # needs an attached session (default "hub")
command = ["./summarize.py"]       # optional: else the top-level command
```

- **Run**: cwd = the plugin dir, own process group, stdin = one JSON document `{"version":1,"type":"event"|"action","plugin","event"|"action","at","hub":{"version"},"payload"}` (`payload.session`: id, name, kind, state, workspace(_id), cwd, worktree, branch, agent; `status.changed` adds `status`, `previous`, `detail`; `session.exited` `exit`; `review.*` `dir`, `head`, `branch`, `base`, `url`, `summary`; `fanout.finished` has no `session`, see below; `share.*` and `record.*` below). Environment: only `PATH HOME USER LOGNAME SHELL LANG LANGUAGE LC_ALL LC_CTYPE LC_MESSAGES TZ TMPDIR TERM XDG_* DISPLAY WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS` from the hub, plus `FW_HUB_PLUGIN`, `FW_HUB_PLUGIN_DIR`, `FW_HUB_PLUGIN_TRIGGER` (`event:<name>` / `action:<id>`), `FW_HUB_STATE_DIR` -- never `FW_HUB_TOKEN`, peer/report secrets, or DSNs (a plugin can still read the token file: it is you). At the deadline SIGTERM, then SIGKILL 0.5 s later, to the whole group; a run also ends 0.5 s after its command does even if something it started left the group (`setsid`) and holds its output open (that process is not killed: the run's error says so). An action keeps running, and is logged, if its HTTP caller hangs up. stdout/stderr: 64 KiB each kept (a JSON response over that is an error). At most `plugins.max_concurrent` runs at once; 64 waiting, then events are dropped (logged).
- **Response**: stdout that is a JSON object: `{"toast": "text" | {"message","tone":"info|ok|warn|error"}}` (events: shown in every open UI; actions: to the caller), and for actions only `{"paste":{"text","run"?,"session"?}}` (into the action's session unless `session`; as `POST /paste`, 64 KiB max). Anything else on stdout is just logged; a malformed object is the run's error.
- **Logs**: every run to `<state-dir>/plugin-logs/<name>.jsonl` (0600; trigger, exit / signal, `timed_out`, duration, error, 4 KiB of each stream, response; trimmed to the newest 200 past 512 KiB) and failures to the hub log. `fw hub plugin logs NAME`, Settings' logs, `GET /api/plugins/{name}/logs`.
- Re-scanned every 2 s (new, changed, removed, enabled); an invalid `plugin.toml` is listed with its error and never run.
- **`fanout.finished`**: a grading left a verified fan-out settled (no agent at work, nothing queued or grading); once per settling (a later re-grade or fix that settles it again announces again). `payload.fanout`: `{"id","name","workspace_id","workspace","folder","repo" (deprecated mirror of folder),"base","base_sha","graded_at","leader","winner","counts":{"candidates","passing","failing","grading","running"},"warnings","candidates":[...]}`, candidates best first: `{"label","rank","verdict":"pass|fail|stale|pending|none","passed","kind","model","session_id","session","branch","path","head","graders_passed","graders_total","diff":{"files","insertions","deletions","untracked"},"failure","tail","cost_usd","tokens","why","pickable"}`. No grader output but `tail`: the last 400 characters of the first failing grader's stderr (stdout if stderr is empty); the board has the rest. It has no session: a subscription with `kinds` never matches it.
- **`share.created`** / **`share.revoked`** (a link minted; each link a revoke ends, one event per link; expiry sends none): `payload.share` `{"id","session_id","name","created","expires"}` (unix seconds) and `payload.session`. Never the credential, its digest, or a link (the link is the credential).
- **`record.started`** / **`record.stopped`**: `payload.recording` `{"name","session_id","title","part","size","started","modified","cols","rows","active"}` (the current part's file in `recordings/`; no path) and `payload.session` (`null` once removed); `record.stopped` adds `reason`: `stopped` (by hand) or `ended` (its process exited, or writing failed). A hub stopping, or a handoff carrying it on, sends none.

| Method | Path | |
|---|---|---|
| GET | `/plugins` | `{"enabled","dir","events","plugins":[{"name","dir","enabled","valid","error","description","version","timeout_secs","events","actions":[{"id","title","context"}],"last_run","last_error"}]}` |
| PUT | `/plugins/{name}` | `{"enabled"}` -> the plugin |
| GET | `/plugins/{name}/logs[?limit=N]` | `{"plugin","runs":[...]}` newest first (max 200) |
| POST | `/plugins/{name}/actions/{id}` | `{"session_id"?}` -> `{"ok","toast","paste","log"}`; 404 unknown, 409 disabled / plugins off / needs a session |

Events socket: `{"type":"plugins"}` (re-list), `{"type":"plugin_toast","plugin","message","tone"}`. Peer credentials are refused on `/api/plugins*`, `/api/settings*`, `/api/config`.

## Remote access (phone, other machines)

Recommended -- Tailscale (HTTPS on the tailnet, which phones need for the clipboard API):

```bash
fw hub serve                                     # loopback only
tailscale serve --bg http://127.0.0.1:7420       # https://<machine>.<tailnet>.ts.net -> the hub
fw hub url --qr                                  # scan with the phone
```

- `serve` and `url` run `tailscale status --json` / `tailscale serve status --json` (3 s timeout; `tailscale` on `PATH` or the macOS app): a `serve` whose `/` proxies to the hub's port is listed and, when found at startup, accepted as an origin. Tailnet up but nothing served: they print the `tailscale serve` command.
- Behind any reverse proxy on this machine, `X-Forwarded-For` (last entry), `X-Forwarded-Host` (counts as same-origin), and `X-Forwarded-Proto` are believed only from loopback peers (IPv4-mapped ones under `--bind ::` included) and `--trusted-proxy` addresses. A proxy elsewhere: pass `--public-url` and `--trusted-proxy`. `tailscale serve` keeps `Host` and sets the forwarded headers, so starting it after `fw hub serve` works (it is just not listed as an origin until the next start).
- LAN without Tailscale: `fw hub serve --bind 0.0.0.0 --tls auto`, then `fw hub url --qr` (next section). Plain `--bind 0.0.0.0` warns: plain HTTP, so phones get no installable app, notifications, or clipboard.

## HTTPS without Tailscale

**The local CA is name-constrained.** `--tls auto` makes a CA whose certificate carries a critical X.509 `nameConstraints` extension permitting only this hub's names and addresses (as they were when it was made). X.509 reads a DNS constraint as that name **and every name under it**, so a bare single-label host name is never a constraint (a host called `dev` would cover all of `.dev`): the CA gets `<host>.local` instead, and `tls_names` refuses single labels. A device that installs it trusts it for those names (and their subdomains) only: even a stolen CA key cannot impersonate your bank or any other site to that device. A CA made before this (constrained to a bare label) is flagged by `serve` and `doctor`: `fw hub tls rotate`. rustls (tested) and current Chrome and Firefox enforce it; still treat it like any root you install: only on devices you use with this hub, after checking the fingerprint against `fw hub tls ca`.

```bash
fw hub serve --bind 0.0.0.0 --tls auto     # or hub.toml [server] tls = "auto"
fw hub url --qr                            # two QRs: install the CA (first time, http /ca) / log in (https)
fw hub tls ca                              # fingerprint, constraints, file (--out FILE [--der] for a copy)
fw hub tls rotate                          # new CA (old key deleted); every device re-installs
```

- **One port, both schemes**: each connection is https or http by its first byte. With TLS on, plain http from the network reaches only `/ca` (plus its fonts/icon); everything else gets `308` to the same URL over https (`Cache-Control: no-store`; no HSTS: it would pin the whole host name). This machine (loopback, or its own address as the source) and `--trusted-proxy` peers keep plain http (already a secure context / the proxy terminates TLS), so `tailscale serve`, a local browser, and the CLI work unchanged.
- **Files** (`<state-dir>/tls/`, dir 0700): `ca-key.pem` (0600, never leaves it), `ca-cert.pem`. ECDSA P-256, CA valid 10 years. Leaf: every address `url` prints plus `localhost`, the host name when dotted (else `<host>.local`), the tailnet name/IPs, `tls_names`; valid 14 days, re-issued 5 days before expiry and whenever the names change (checked every 60 s). Served chain: leaf, CA.
- **A name outside the constraints** (a new DHCP lease, a WSL port-forward's Windows IP): left out of the leaf with a log warning, the rest keeps working. Cover future leases up front with `tls_names = ["192.168.0.0/16"]` (a network, at most /8), then `fw hub tls rotate`.
- **`/ca`** (public, read-only): the fingerprint to compare out of band, downloads (`/ca/hub-ca.crt` DER, `.pem`, `.mobileconfig` iOS profile), steps per OS (iOS: Settings > Profile Downloaded > Install, then General > About > Certificate Trust Settings; Android: Settings > Encryption & credentials > Install a certificate > CA certificate; Windows/WSL browsers: Trusted Root Certification Authorities, current user; macOS: login keychain, Always Trust; Linux: NSS `certutil` / `update-ca-certificates`). Its script probes `https://<same host>/ca/probe` and says whether this device trusts the CA; it never navigates by itself (a trusting device can still compare the fingerprint) and never sees a credential.
- **QR with TLS**: two codes. `http://<lan>:<port>/ca` (no credential: the page is plain http, which an active LAN attacker could rewrite, so check the fingerprint) for a device's first time, then `https://<lan>:<port>/login?code=<one-time code>`. The login code only ever goes to the https origin.
- **Rotation is revocation**: `tls rotate` deletes the old key (nothing new is ever signed by it) and a running hub switches at once (`POST /api/tls/reload`): it closes every TLS connection made under the old CA and resumes no sessions (the hub and its clients disable TLS session resumption), so a device or machine still trusting only the old CA fails its next request. Remove the old CA from devices; machines pinned to it must re-pin (the error names the new CA fingerprint).
- **Cookies/origins**: logins over the hub's TLS get `Secure`; over https the hub's origin is `https://...` (an `http://` Origin on the same host is refused for state changes).
- **CLI and other hubs**: the local CLI uses plain http to loopback or the specific `--bind` address (traffic that never leaves this machine), so it works whatever names the certificate carries and across rotation. `--url https://...` (attach, ls, ...) trusts the system roots plus this state dir's CA, or `--fingerprint <CA SHA-256>` from the hub's `fw hub tls ca`. `machine add --fingerprint` takes the same CA fingerprint and survives leaf renewals (the chain is verified against that CA: names, expiry, constraints).
- **`--tls CERT,KEY`**: PEM chain + key you supply (absolute paths in hub.toml), re-read when they change; no local CA, `/ca` answers 404.
- `GET /api/tls` (signed in; peers refused): `{"mode","ca_fingerprint","constraints","names","outside","expires"}`.

## Machines

One browser UI over several hubs (herdr's "connecting machines"). This hub (A) registers other hubs (B) and relays the browser's API calls and WebSockets to them under `/m/<machine>/api/...`, so the browser talks to A's origin only (one `tailscale serve` covers the phone) and never holds a remote credential.

```bash
# on B: a peer credential for A (printed once, alone on stdout)
fw hub peer issue --name laptop > /tmp/cred
# on A: register B (never pass secrets on argv)
fw hub machine add desktop https://desktop.tailnet.ts.net < /tmp/cred
FW_HUB_MACHINE_SECRET=$(ssh desktop fw hub token) fw hub machine add desktop URL --mint   # or: B's token, used once to mint one; not stored
fw hub ls --machine desktop; fw hub attach --machine desktop SESSION
```

- **Peer credential** (`fwpeer_<id>_<secret>`, 256 bits; digests in B's `peers.json`, 0600): API and WebSockets only -- never a browser login. **Equivalent to B's token**: it starts sessions running any command as B's user, so it can read B's token file. Allowed: what drives sessions, search and transcripts, templates, and usage included. Refused (403, defense in depth, not a boundary): `/api/token/rotate`, `/api/login-codes`, `/api/peers*`, `/api/machines*`, `/api/workspaces/import`, `/api/code-workspaces`, `/api/plugins*`, `/api/settings*`, `/api/config`, `/api/shares*`, `/api/recordings*`, `/api/sessions/{id}/record/*`, `/m/*`, and every non-`/api` page. Survives B's token rotation; `fw hub peer revoke` ends its requests at once and closes its sockets (4003). `peer issue/ls/revoke` go through B's running hub, else edit the file (a running hub re-reads it on change).
- **A's side**: `machines.json` (0600, state dir 0700) holds name, URL, peer credential, optional pin. Plaintext on purpose: an OS keyring (Secret Service, Keychain) hands secrets to any process of the same user once unlocked -- exactly who can already read the file (and the hub token beside it, and every session) -- is absent on headless boxes and WSL where hubs usually run, and its Linux client (`keyring` over Secret Service) would add a D-Bus client stack to the static musl release builds for no gain against that threat. Protect the state dir as you would `~/.ssh`: full-disk encryption against a lost disk; keep it out of unencrypted backups. `machine ls` names the store (`credentials: <path> (plaintext file, mode 0600)`; `--json`: `credential_store`). `add` checks the credential with `GET /api/whoami` there and refuses a hub token, this hub itself, and a hub already registered (same URL, hub id, or peer; a credential it minted for a refused add is revoked). `add --replace` (UI: Re-pair) swaps a machine's credential, URL, or pin keeping its name, and revokes the old credential. `rm` asks B to revoke the credential (`DELETE /api/peer`; else it says how). Names: `[a-z0-9-]`, 1-32, not `local`.
- **Proxy**: only `/api/...` on a registered machine's base URL (SSRF guard; redirects, cookies refused/stripped). Request bodies up to 8 MiB, answers up to 32 MiB, 120 s per call; answers are served as JSON, plain text, or opaque bytes only, with `Content-Disposition: attachment` and CSP `sandbox` (a machine's answer never runs as this hub's page). A machine's 401 becomes 502 `{"error","machine","machine_state":"refused"}` (never signs the browser out); unreachable is 502 `offline`. After a refusal A answers for 90 s without asking (B's failure limit counts A's address). WebSockets relay frame by frame with backpressure; B's 4001-4003 reach the browser as 4010, a removed machine closes with 4011, a lost link 1011.
- **TLS**: https machines are verified against the system roots, or by `--fingerprint`: the SHA-256 of a hub's local CA (`fw hub tls ca` on it; the chain is verified against it, names and expiry included) or of a self-signed leaf (`openssl x509 -noout -fingerprint -sha256`; names and expiry not checked, like an SSH host key). An untrusted certificate's refusal names the fingerprint to pin (the top of the chain presented).
- **UI**: the sidebar groups workspaces by machine (this hub first), each head with its link state (online / connecting / offline with retry countdown / refused) and Reconnect; each machine has its own events socket with independent backoff (0.5 s doubling to 30 s), and keeps its last lists while unreachable. Needs you, notifications, tab counts, attach, layout, commands, review, worktrees, launcher (templates live on, and launch on, the machine; cost chips come from its `/api/usage`), search (see Search), and the Flywheel panel all work for its sessions; a machine badge marks them. The sidebar's spend strip is this hub's only and is labelled with its host once machines show. New workspaces pick a machine (VS Code import: this hub only). **Machines** (sidebar footer, palette) adds, tests, and removes them.

| Method | Path | |
|---|---|---|
| GET | `/whoami` | `{"scope":"full"\|"peer","peer","version","host","hub_id"}` |
| GET / POST | `/peers` | list `{"peers":[{"id","name","created","last_used"}]}` / `{"name"}` -> 201 `{"peer","credential"}` |
| DELETE | `/peers/{id or name}` | revoke; `DELETE /peer`: a peer credential revokes itself |
| GET / POST | `/machines` | `{"host","machines":[view]}` / `{"name","url","credential"\|"token","peer_name"?,"fingerprint"?,"replace"?}` -> 201 view (200 on `replace`) (`{"name","url","fingerprint","peer","added","status":{"state","since","error","version","host","presented_fingerprint"}}`) |
| DELETE | `/machines/{name}` | `{"machine","revoked","note"}` |
| POST | `/machines/{name}/test` | `{"ok","latency_ms","whoami"\|"error","machine"}` |

## Share links

**A viewer sees everything that terminal shows** -- output, typed secrets echoed back, the scrollback in the snapshot. Nothing is redacted; share a session you would screen-share.

```bash
fw hub share agent --expires 2h --name pairing   # URLs (every origin, remote first) to /share/fwview_<id>_<secret>; --qr for a phone
fw hub share ls; fw hub share revoke pairing     # or the id; --all [--session S]
```

- Credential: `fwview_<id>_<secret>` (256 bits) in the link's path; digests only in `<state-dir>/shares.json` (0600, survives restarts and upgrades). Expiry 60 s to `shares.max_expiry` (30 d unless set; default `shares.default_expiry`, 1 h unless set; `GET /api/shares` has `policy` `{"default_expires_in","max_expires_in"}` and the UI's picker follows it); labels unique among live links; at most 128 live. Minted by the token or a login (`POST /api/shares`; a peer credential gets 403).
- Reaches only: `/share/<cred>` (viewer page: terminal, title, status, expiry; no sidebar, no input), `/share/<cred>/info` (`{"share":{"name","expires","now"},"session":{"name","workspace","kind","state","code","signal","title","agent","live","cols","rows"}|null}`), `/share/<cred>/ws` (the terminal socket, read-only), and its own assets (`viewer.js`, `term-view.js`, `viewer.css`, xterm). Any other path under it, and the credential as a bearer anywhere (`/api/...`, `/`, `/assets/...`): 403.
- Read-only socket: the snapshot, output, `size`, `exit`, `reset` as on `/api/sessions/{id}/ws`; every `input`/`resize`/`theme`/binary message is refused (`{"type":"error"}`), nothing reaches the PTY. It never sizes the PTY, never counts as viewing (seen/notify) or in `live.clients` (it is `live.link_viewers`). Cross-origin upgrades refused.
- Revoke or expiry closes open views at once (code **4004**); the link then answers 404 (revoked) / 410 (expired). Removing a session revokes its links (`share.revoked` each). A live handoff closes them with 1012 and the viewer reconnects. Token rotation leaves links alone (revoke them).
- Admission: a request whose path carries a live link's credential holds a **share-link slot** (its own pool, sized like the unauthenticated one, 8 per client; kept alive; full: 429 at once), never a main slot, so a link holder cannot lock the operator out. At most 32 open views per link and 256 in all (429). A wrong credential is a failure for the failure limit.
- Viewer: `fit` / `readable` toggle (fit the whole screen to the window, or a readable font that scrolls; auto picks readable when fitting would go under 8 px; remembered per browser). After revoke/expiry it says the screen is no longer live.
- UI: the `share` header pill (count = link viewers now), palette "Share <session>", sidebar `sharing` chip: mint (label, expiry; copied), list with viewers, revoke. This hub's sessions only (a machine's: share there).

## Recording

```bash
fw hub record start agent        # also the `rec` header pill
fw hub record stop agent; fw hub record ls
fw hub record get <name> -o demo.cast; asciinema play demo.cast   # or Play in the UI: /replay#<name>
```

- Opt-in per session; `<state-dir>/recordings/<session name>-<UTC time>.cast` (0600; the name as `[A-Za-z0-9_-]`, at most 48), asciinema v2: header (`width`, `height`, `timestamp`, `title` = `workspace/session`, `x_fw_hub` `{session_id, part}`), then `[t, "o", text]` output (UTF-8; a split sequence waits for its end, invalid bytes are U+FFFD), `[t, "r", "COLSxROWS"]` resizes, `[t, "m", label]` markers (`exit: N`, `resync` after falling behind, `continued in <part>`). Starts with the screen as it is (screen-only snapshot at t=0); times are monotonic.
- Caps: `recordings.part_cap` per file (64 MiB unless set; the closing marker included), then it rotates to `<base>.part<N>.cast` (a fresh snapshot first); `recordings.dir_cap` for the directory (1 GiB unless set), oldest finished recordings deleted first. Both live: a hub.toml edit reaches running recordings at their next event.
- Through this hub only: `record` verbs take no `--machine`, and a peer credential gets 403 on `/api/recordings*` and `/api/sessions/{id}/record/*`.
- Ends when stopped, when the process exits (or is relaunched), or when the hub stops (written out and closed). `fw hub upgrade` keeps it going in the same file: the old hub writes out what was output and lets go (`recordings/active.json`), the new one reopens it in append mode before the sessions resume (no gap). A normal restart does not resume recordings.
- A recorder is not a viewer (`live.clients`). An active recording downloads as written so far; delete refuses it (409: stop first). A stop writes what was queued when it was seen, then closes (bounded under sustained output).
- Listed with a recording's parts together, in order. `record get` writes 0600. CLI times are UTC with a relative offset.
- Replay player (`/replay#<name>`, signed-in browsers only): play/pause (space), seek (arrows +/-5 s; markers as ticks on the bar, click to seek), speed 0.5-8x, skip idle (gaps over 2 s play as 2 s), `fit`/`readable`, download, prev/next part links; it continues into the next part at the end.

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
- Persisted to `<state-dir>/state.json` (0600, atomic writes; schema version 4 -- 2 added layouts, 3 launch templates, 4 fan-outs; older files migrate on load, newer ones are refused). Restarts restore sessions (above) and keep each workspace's layout (below).
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

## Review

Any session in a git folder (worktree sessions, their companions, plain sessions in a repo) can be reviewed from the browser, a phone, or the CLI: header pill `review` / `Alt G` / palette. Desktop: a drawer over the terminal (never resizes the PTY); phones: a full-screen sheet.

- **Base**: a worktree's base; else the branch's upstream; `?base=` / `--base` overrides (400 if it starts with `-` or names no commit).
- **Shows**: commits on HEAD the base lacks (newest 200; `ahead`/`behind`), committed files vs the merge base with +/- (renames, binary), and the working tree apart: conflicts, staged, unstaged, untracked; upstream sync, remotes (credentials masked), an operation in progress.
- **Diffs**: per file (a path the scope does not change is a 400; a rename is diffed as one, asked by either name; a conflicted file shows git's combined diff), lazily, unified or side by side (desktop >= 1000 px; phones unified), light syntax colouring; binary files and changes over 400 lines start collapsed; 2000 lines per page with Load more; at most 8 MiB read per file. Re-read when the session's git badge changes.
- **Actions** -- each a dry run first showing the exact commands (and where they run), what they touch, and why not; running needs that plan's `fingerprint`, and a repository that moved since is refused (409, re-plan):
  - **Commit all**: `git add -A` then `git commit -F -` (message on stdin; hooks run). What `add` staged must match the plan (same paths, same contents, nothing left unstaged), else the index is put back and nothing is committed. "Ask the agent" pastes a prompt (shown first) into the session's agent asking for a message in a `text` block; the first new block fills the editor.
  - **Push**: `git push <remote> <planned HEAD>:refs/heads/B` (then `git branch --set-upstream-to=<remote>/B B` where `-u` applies) -- the commit reviewed, never one added since; always the same-named branch (a branch tracking `origin/main` is never pushed onto it): push remote, `remote.pushDefault`, upstream's, `origin`, else the only one (or picked). The upstream is set only when it tracks nothing, or another branch on that remote. Never forced; a diverged remote is git's refusal, shown. New worktree branches are cut `--no-track`.
  - **Open PR**: `gh pr create --base --head B --title --body-file -` from the pushed same-named branch; the fingerprint covers title, body, base and draft. Refused when not pushed / out of sync, the remote has no host, or `gh` is not installed or not signed in for that host (`gh auth status --hostname`).
  - **Rebase onto base** (worktrees): `git rebase <base>` in the worktree; refused with uncommitted changes. A conflict stops it there, left in progress; the agent can be asked to resolve and `--continue`, or **Abort** (`git rebase --abort`; `POST .../review/abort`). From the merge dialog, a successful rebase returns to the merge.
  - **Merge into base** (worktrees): `git -C <main checkout> merge --ff-only <planned B commit>` (a commit landing on B after the plan is not merged). Refused when the main checkout is not on the base, has uncommitted tracked changes or an operation in progress, the base is a commit, or it is not a fast-forward (`needs_rebase`: rebase first). Then offers removing the worktree (the usual close dialog: nothing lost, merged branch deleted).
- **Staleness**: commit/rebase/abort fingerprints hash `git status --porcelain=v2` plus the content of every changed and untracked file (files over 64 MiB: size and mtime).
- **Runner**: argv only, no shell; own session (no terminal: nothing prompts), `GIT_TERMINAL_PROMPT=0`, editors `true`; output capped; deadlines (reads 20 s, commit/push 5 min, rebase/merge 2 min, gh 60 s) kill the process group; at most 4 git/gh at once hub-wide; one action per repository at a time. Every action, ask, plan refusal (once until it changes), and worktree removal is logged to the hub log (commands, not messages).

## Handoff and peer review

Claude Code and Codex side by side, working together through the hub. Hub module `agent_handoff.rs` (not `handoff.rs`: that is `fw hub upgrade`'s).

- **Send to agent**: part of one session -> another agent session (any workspace, any machine) as one prompt: `From <session> on <branch>: <what>` (`in <dir>` without a branch; `(machine <host>)` when the target is on another machine), then the content in one fence (longer than any backtick run inside), then `[truncated by fw hub: kept the first N of M bytes]` when cut (default 32 KiB, line boundary). Sources: the terminal selection (palette, `handoff.send` key, phone selection popover), a transcript message or the part of it selected (viewer's `To agent`), a suggested block (Commands drawer), the branch diff or one file's (Review drawer). The hub adds only the header and fence: nothing but what you picked. Composed on the source's hub, edited in the dialog (`Ctrl Enter` sends), delivered on the target's hub (bracketed paste, then Enter); a machine is reached through `/m/<machine>`. CLI: `fw hub handoff FROM TO` (`--machine M`: FROM on M; `--to-machine N|local`: TO on N, or this hub). An agent session restored as a plain shell takes no handoff (409): kill and relaunch it.
- **Ask for a review** (Review drawer, palette, `review.ask` key, `fw hub ask-review`): a reviewer of the other kind (`review.reviewer`; a shell's work gets Claude; the other kind when that one is not installed; `--kind` must be installed) in the author's directory and worktree, read-only: Claude `--permission-mode plan`, Codex `-a never -s read-only`. First prompt: review `git diff <base>...HEAD` (cut at 24 KiB, with how to read the rest), uncommitted changes noted, findings with file:line, commands and changes in fences. A running paired reviewer of that kind is asked again (pasted); else a new one starts and the one it replaces (stopped, or another kind) is closed: one reviewer per author. Requests are serialized (a double click asks the first reviewer again). 409 when nothing is committed or changed.
- **Pairing**: the author's `reviewer`, the reviewer's `review_of` (state.json; survives restarts). Sidebar: the reviewer hangs under its author, with linked chips; `fw hub ls`: `(reviewed by R)` / `(reviews A)`. Closing the author closes its reviewers; a reviewer never removes the worktree.
- **Findings**: the reviewer's text after its last prompt (its transcript; Codex without a rollout: its newest reply's blocks), its code blocks with Copy / To author, and **Send findings to <author>** (editable) under `From <reviewer> on <branch>: review findings (...)`.
- **Events**: `review.requested` `{"author","reviewer","reused","kind","base","branch","dir"}`; `handoff.sent` `{"from","to","source","bytes","truncated"}` (on the target's hub; findings: `source` `review_findings`).

## Verified fan-out

One prompt to 2-6 agents (claude and/or codex, a model each), each in its own sibling worktree cut from **the same base commit**, plus grader commands the **hub** runs -- the flywheel loop's verification, beside the terminals. UI: sidebar **Fan-out**, palette "New verified fan-out"; CLI: `fw hub fanout`.

```bash
id=$(fw hub fanout new demo --prompt "Fix the flaky retry test" \
  --agent claude:opus --agent claude:sonnet --agent codex:gpt-5.1-codex \
  --grader 'unit=cargo test -p retry' --grader ~/oracles/retry-check.sh --permission-mode acceptEdits)
fw hub fanout wait "$id" --timeout 3600; fw hub fanout show "$id" -v
fw hub fanout fix "$id" c3 --yes           # paste c3's failing grader output into its agent
fw hub fanout pick "$id" c1 --yes [--rebase]
fw hub fanout cleanup "$id" [--force] --yes
```

- **Agent claims are untrusted**: a candidate's finished turn (Claude `Stop`, Codex `notify`), or its agent's process ending, only *queues* grading; the verdict is the graders' exit codes. **Held out of the prompt only**: agents run as your user, so one could read them from the hub's state or API, and Ask to fix shows the failing command. What is enforced: they run in the worktree, so a grader the repo tracks (`cargo test`) is editable by the agent -- list the paths it reads under `--protect` (compared by content hash against the base: a candidate changing one fails), or put the oracle outside the repo (as with the [held-out gate](held-out-gate.md)). A grader's absolute-path script (<= 1 MiB) is pinned by hash at launch (changed later, every grading fails); a relative path in the repo (`./scripts/check.sh`) is added to `--protect`. Tracked files flagged skip-worktree / assume-unchanged fail a grading (git status would not show them).
- **Grading** = flywheel-core's command-grader runner (injected by `fw`, same semantics as a task's `command` graders): in order, `/bin/sh -c` in its own process group, pass = exit 0, the group killed at `--timeout` (default 900 s each), the first failure skips the rest, a failing grader's stdout/stderr tails kept (8 KiB each; a pass keeps none). At most 2 gradings run at once, hub-wide. Recorded per candidate: verdict, each grader's status / exit / duration / output, HEAD and `git status` at the start (a worktree that changes meanwhile is flagged), diff from where it left the base (commits + uncommitted + untracked lines), cost and tokens. A new finished turn grades again (after Ask to fix). Re-grade any time.
- **Ranking**: passing first (smaller diff, then lower cost; a pass that changes nothing ranks last among them, cannot be picked, and the board warns that the graders pass on the base unchanged), then failing (more graders passing, smaller diff, lower cost), then stale (changed since graded), grading, and not yet graded. Each card says why (`below c1: larger diff (31 vs 1 lines)`).
- **Pick winner** (needs a fresh pass graded on a clean tree, the worktree still on its fan-out branch, and that branch at exactly the commit that passed -- what lands is what was graded): the review merge (`git -C <main checkout> merge --ff-only <commit>`, plan shown first, fingerprint-confirmed). Base moved since: offered a rebase (`--rebase`), which grades again; pick once it passes. One winner per fan-out.
- **Cleanup**: remove the others' worktrees and sessions (default: all but the winner), refused per candidate when it holds work nothing else has (commits not in the base, uncommitted or untracked files) unless `--force` (commits stay on their branch); or `--stop-only` (hang up the sessions, keep the worktrees; an agent that never finished is then graded as it is). After the pick the board offers to remove the winner's merged worktree too. `rm` forgets the record only.
- **Ask to fix** pastes the first failing grader's command, exit, and output tail (or the protected paths changed) into that candidate's agent, then Enter.
- **Kept** in `state.json` (schema 4; at most 50 fan-outs). A grading cut short by `fw hub stop` or a live handoff (`fw hub upgrade` pauses gradings) runs again in the hub that carries on; a turn finished meanwhile is graded on start.
- **Needs you**: a fan-out whose graders finished joins the list (green when one passes, after blocked agents) until its board is opened in that browser; its candidates' finished turns are not listed separately. Candidate sessions are named `c1-<name>-<tag>`. A launch that fails part-way removes its sessions, worktrees, and branches. Phone: the board is a full-screen sheet; Pick winner and the cleanup sheet work there.
- This hub's fan-outs only (another machine's live on its own hub).

### Fan-outs and flywheel tasks

A fan-out is a one-off; a flywheel task is the durable spec + grader asset the loop re-verifies. Both ways, for a folder flywheel drives with a **directory** work source:

```bash
fw hub fanout promote "$id"            # shows the path and the exact bytes; asks
fw hub fanout promote "$id" --yes      # writes .flywheel/tasks/active/NN-<slug>/<slug>-<tag>.json
fw hub fanout new demo --from-task fix-retry --agent claude:opus --agent claude:sonnet
```

- **Promote** (board **Promote**): goal = `--goal`, else the seeding task's, else the prompt's first line; the whole prompt -> `context.notes`; graders -> `command` graders in order; `--protect` paths -> `context.constraints` lines (the plan names those flywheel.toml's `[submit] protected_paths` does not cover: only that list refuses a landing, and the hub never edits flywheel.toml); the winner's commit (or `--reference CAND`, a passing candidate) -> one `context.references` line, prior work to consult, not the solution. Validated and written by the `flywheel` crate through core's task loader (`hub_tasks.rs`), read back through the directory source. Phase default: a new `NN-<slug>` after every active/archived one.
- **Where it lands**: a new **uncommitted** file in the fan-out's main checkout, as `/fw-plan` and `fw init` do; `fw worker` reads the working tree, and the hub never commits to your branch. Refused: an existing file, an id already defined (active or archived), or an id with runs in the store (runs are keyed by task id). Dry run first; the write confirms the plan's fingerprint. Then **Run with worker here** (Start worker) so the loop verifies it again.
- **Fan out this task** (Flywheel panel, a task's **fan out**): prompt = the task's goal, context, and command graders (`# Goal` / `# Context` / `# Graders`); graders = its command graders (editable), then its **held-out** graders (`[held_out] root`: its registration plus matching standing oracles), added server-side at launch. Held-out graders run in every worktree but never reach an agent: a prompt naming one is refused, **Ask to fix** says only that a held-out check failed (no command, no output), and promoting back leaves them in the registry (keyed by the original task id: register them for a new id with `/fw-verify`). A registry that cannot be read refuses the launch (fail closed). Rubric / manual / transcript graders are not run by the hub (listed; the worker still applies them).

## Layout: tabs and split panes

Each workspace has a **layout**: named tabs, each a tree of splits (side by side or stacked, any ratio) whose leaves are panes showing one session. Stored on the hub (`state.json`, per workspace, revisioned), shared by every browser; which tab is active, the focused pane, and zoom are per browser.

- **Desktop**: a tab bar over the stage (`+` new tab, double-click renames, right-click menu, middle-click closes); each pane has its own terminal, socket, reconnect banner, and title bar (split right / down, zoom, menu, close). Click or type in a pane to focus it (signal ring). Drag a divider (or focus it and use arrows, `Shift` for 10%, double-click for 50/50). Drag a session from the sidebar onto a pane: an edge splits that way, the middle shows it there. Opening a session focuses the pane already showing it (switching tabs), else shows it in the focused pane; the new-session form's **Show in** picks focused pane / split right / split down / new tab.
- **Splitting** with a new shell (`Alt \`, `Alt -`, title bar, menu): an agent's companion shell first (its directory, worktree included), else a shell in the session's folder.
- **Companion flip** (``Alt ` ``) on screens over 1100 px opens the companion beside the agent (focuses it when on screen; from a zoomed pane the zoom moves to it); palette "Companion shell: in place" restores the old swap.
- **Minimum pane size**: a split never leaves a pane under 40x8 cells. Without room the new session opens in a new tab (toast says so); the companion flip swaps in place instead.
- **Closing**: a pane (`Alt X`) only leaves the layout; its session keeps running. The last pane closes its tab (an only tab stays, empty). Closing or removing a session closes every pane showing it, and any tab that leaves empty (again an only tab stays; the hub prunes layouts, pushed as `layout`).
- **Cost**: only the active tab's panes are attached (other tabs: no socket, no terminal; switching re-attaches from the snapshot); panes behind a zoom stay attached but are not drawn. xterm's DOM renderer, no WebGL/canvas contexts. At most 12 tabs per workspace and 12 panes per tab.
- **Phones** (under 760 px): no splits. One pane full screen; a chip row switches between the layout's sessions (one chip per session, `tab session`, the current one scrolled into view); phones never change layouts.
- **Seen**: every pane on screen counts as viewed (`view` carries `visible`); panes behind a zoom or in other tabs do not.
- **PTY size** (last resizer wins): a pane resizes its session's PTY only while focused or while it is the session's only viewer, once a divider drag ends (not per move). Otherwise it follows the other viewer's size (`NNxNN` badge), clipped to its box with the window kept on the cursor and a dashed edge where rows are cut off above or below.
- **Restore**: a page opened without `#session=` reopens the last workspace this browser viewed (else the first with a saved layout) at its focused pane. An empty pane (new tab) offers the workspace's sessions and a new shell.
- **Browser check** (opt-in, not in `scripts/check.sh`): `node crates/flywheel-hub/e2e/layout.mjs [OUT_DIR]` -- temp state dir, free port, fake `claude`; 1440x900 3 panes + 2 tabs + reload + minimum pane size, jumps between two workspaces, a second smaller viewer, 390x844 phone landing, quick-answer parsing, the header with a usage meter (name, status, why? uncovered), every header pill at 390 and 430 px (they wrap, nothing covers the name, status or why?), tap targets (uncovered, 44px tall; icon and second-row controls 44px wide); at 1440x900 the sidebar footer (at most 180px, every utility uncovered and named), why? and usage popovers anchored under their buttons, a long worktree path cut from its start (header, close dialog), a toast over the fan-out board and over a dialog opened after it (on top, clickable, never over the dialog); `/replay` and `/share` tap targets at 390x844 (44x44); screenshots in `OUT_DIR`. Env `FW`, `PLAYWRIGHT` (package path), `CHROMIUM` (executable), `PORT`.

| Key | |
|---|---|
| `Alt \` / `Alt -` | split the focused pane right / down with a new shell |
| `Alt Shift` arrows | focus the pane in that direction |
| `Alt Shift H J K L` | move the focused pane's nearest divider left / down / up / right |
| `Alt Z` / `Alt X` | zoom (this browser) / close pane |
| `Alt T`, `Alt PgUp/PgDn` | new tab, previous / next tab |
| `Ctrl B` then a key | only with the pane prefix on (Settings > Keyboard, or palette): `%` `"` split, arrows focus, `Ctrl` arrows resize, `z` `x` `c` `n` `p` `1`-`9` `,` (rename) `&` (close tab) `o` (next pane) `?`; `Ctrl B` twice sends `^B` |

## Agent status

| Status | Meaning | Claude Code | Codex |
|---|---|---|---|
| `working` | busy | `UserPromptSubmit`, `PreToolUse`, `PostToolUse[Failure]` | screen: its status line (`Working (12s • esc to interrupt)`) |
| `blocked` | waits on a person | `PermissionRequest` (detail: the question, for `AskUserQuestion`); `Notification` `permission_prompt` / `elicitation_dialog` / `elicitation_url_dialog` / `agent_needs_input`; `idle_prompt` while working; screen: the folder-trust dialog before any hook, a known dialog 3 s with no hook | screen: exec / edit / permission / network approvals, `request_user_input` questions, folder trust, legacy `Allow command?` (detail: the dialog's question) |
| `done` | finished a turn **this client** has not viewed | `Stop`, `StopFailure` | `notify` `agent-turn-complete` |
| `idle` | finished and seen, or waiting for a first prompt | `SessionStart`; `done` once viewed; an interrupted turn (below) | `done` once viewed; screen: the composer (`›`) |
| `unknown` | no signal yet, agent ended | `SessionEnd`, exit | exit; a screen-driven turn whose status line froze 5 s (it stays unknown until the agent outputs again), or whose screen matches no rule for 3 s |

- **Interrupts** (Esc) fire no Claude hook. Once a reported `working`/`blocked` spell has shown a running turn or a dialog on screen, and then shows the prompt (or nothing the rules know) for 3 s with no hook, the turn reads as interrupted (`idle`). A reported `blocked` whose dialog closes onto a running turn was approved: `working` again (its `PostToolUse` only comes when the tool finishes). A spell that never showed a turn (a narrow terminal cuts the footer) is left alone; the transcript viewer (`ctrl+o`) holds the status.
- **Precedence**: hook / `notify` reports win; a report landing while the screen is being read wins over that read. The screen fills only what reports miss (rows above). Shells have no status.
- **Idle is not authorization.** It means the screen shows no running turn and no prompt the rules know -- never that anything was approved or is safe to approve. A prompt the rules do not know reads as idle or unknown, never blocked.
- **Seen is per client** (a browser keeps an id in local storage): viewing a session (attached, page visible and focused) turns its `done` into `idle` for that client only; a turn that finishes while a client watches is `idle` for it at once. The REST API counts a turn seen once any client viewed it.
- **Rollup**: a workspace shows its most urgent session: blocked > working > done > idle.
- **Claude**: the hub assigns `--session-id <uuid>` (stored as `agent_session_id`; follows `/clear`) and injects hooks with `--settings '<json>'` (merged with your own settings; `args` form, no shell) that run `<this binary> hub report ...`. Relaunching resumes the conversation (`--resume <id>`) once Claude has a transcript for it (`$CLAUDE_CONFIG_DIR` or `~/.claude`, `projects/*/<id>.jsonl`).
- **Codex**: launched with `-c notify=["<this binary>","hub","report","--source","codex","--event","notify"]`; its `thread-id` becomes `agent_session_id`.
- **Channel**: hooks report over `<state-dir>/report.sock` (unix socket, 0600, never on the network whatever `--bind` says; a state dir too long for a socket path uses `$TMPDIR/flywheel-hub-<uid>/<digest>.sock`, dir 0700) with a per-spawn random secret that authorizes status for that one session only. Reports are advisory UI state: they cannot start, stop, or change a session.
- **Not a sandbox**: the hub's token is never handed to agents (argv or env), but agents run as your user and can read `<state-dir>/token` like any of your files. Run untrusted agents under another user or in a container.

## Detection manifests

Per-kind TOML screen rules (herdr's model), read against the **live bottom** of the server's emulator screen (never a client's scrolled view). Bundled: `crates/flywheel-hub/detect/{claude,codex,shell}.toml` (embedded). Override: `<state-dir>/agent-detection/<kind>.toml` replaces that kind's file wholesale; validated at load, re-read within 1 s of a change (no restart); an invalid file is logged and listed by `explain` / the UI, and the bundled one stays in force.

```toml
kind = "codex"            # must match the file name
version = 2               # yours; shown by explain
[[rule]]                  # tried in order; the first whose every condition holds wins
id = "approval"           # [a-z0-9_-]
state = "blocked"         # working | blocked | idle | hold (keep the status, e.g. a transcript viewer)
detail = "asks for approval"                        # blocked only
detail_line = ['(?i)^\s*(would you like to .+\?)$', '^\s*(\$ .+)$']   # blocked only, wins over detail: read in turn down the screen, each below the last; capture 1 of each is joined (a pattern with no group only marks a row)
[[rule.when]]             # all clauses must hold (or put the keys on the rule itself: one clause)
lines = 1                 # the last N non-empty rows (1-100, default 30)
line = ['(?i)^\s*press enter to confirm or esc to cancel\s*$']   # each regex matches some row
[[rule.when]]
lines = 100
any = ["would you like to run the following command?"]   # also: all, none (substrings, case-insensitive, one row), not_line (regex)
```

- **Rows**: the server's screen grows taller the way xterm.js does (scrollback comes back only with the cursor on the bottom row), so evidence row numbers are the rows a viewer sees.
- **Strict blocked**: bundled `blocked` rules pin a dialog's footer to the last row plus its question/options; quoted dialog text in an answer or prompt does not match. Write overrides the same way.
- **Cost**: read only after output (at most every 250 ms, once more when it stops) and while a settle is pending, by one thread per agent session; patterns compiled once per load (`regex`: linear time, 256 KiB program cap); a rule reads at most 100 rows of 512 chars; at most 64 rules, 8 clauses each, 16 patterns per list, 400 chars each, 64 KiB per file.
- **Claude** rules are from Claude Code 2.1.306 screens (fixtures in `detect/fixtures/`); **Codex** rules from the codex TUI's own strings and snapshot tests (`codex-rs/tui`). **Shell** rules (password / `[y/N]` / prompt on the last row) only answer `explain`.
- **Explain**: `fw hub explain S` (`GET /api/sessions/{id}/explain`, the UI's `why?` beside the status pill) gives the status, what set it (`source`: `hook` + agent event, `manifest` + file (`codex.toml` bundled, `agent-detection/codex.toml` override) + rule, `heuristic` + reason, `hub` (process exit)), a fresh read of the screen (rule, detail, evidence rows), every rule with why it did not match, the manifest's origin and version, and override problems.

## Agent launcher

The new-session form (and `fw hub new`) starts an agent with:

- **First prompt**: passed as the CLI's positional prompt after `--` (`claude [opts] -- PROMPT`, `codex [opts] -- PROMPT`), so it is the first user turn (after Claude's folder-trust dialog, if shown). Only on create; a relaunch or restore never re-sends it. Max 32 KiB.
- **Model**: `claude --model` (alias or full id) / `codex -m`; suggestions offered, any id accepted (`[A-Za-z0-9._:/@[]-]`, no leading `-`).
- **Permissions**: Claude `--permission-mode manual|acceptEdits|plan|auto|dontAsk|bypassPermissions` (the UI warns on bypass, `fw hub new` prints a warning, the hub logs it, views carry `unchecked: true`); Codex `-a untrusted|on-failure|on-request|never` and `-s read-only|workspace-write|danger-full-access`. Recorded as the session's `launch`, reapplied on relaunch/restore.
- **Templates** (per workspace, in `state.json`, schema 3; agents only): name, kind, prompt, model, permissions, worktree yes/no. Pick one in the form to fill it, "save as template" / delete there (both ask for a second click), or `fw hub template`. Saving over an existing name is refused unless `--replace` (overwrites every option) or `--edit` (changes only the flags given; `--model ''` clears one). Quick launch: palette "New agent from template: NAME" or the sidebar's **From template** (phone too); an unchecked template is marked, listed last, and asks to confirm. A worktree template gets task `<name-slug>-<4 hex>` unless `new --worktree TASK`. `new --template` flags override the template's; a `--kind` different from the template's keeps only its prompt.

API: `POST /api/sessions` takes `prompt`, `model`, `permission_mode`, `approval`, `sandbox`; `GET /api/templates[?workspace_id=]`, `POST /api/workspaces/{id}/templates` (create; `"replace": true` overwrites the same `name`, else 409; unknown fields are refused), `DELETE /api/workspaces/{id}/templates/{name}`; `GET /api/workspaces` lists each workspace's `templates`.

## Usage and cost

The transcript tailer also counts tokens: Claude's per-response `message.usage` (repeated lines of one message id count once; subagent transcripts `<id>/subagents/*.jsonl` included), Codex's rollout `token_count` events (growth of the cumulative totals; model from `turn_context`). Cost uses the model the transcript records (Claude's plan mode may run a different model than `--model`).

- **Prices**: an embedded table (`crates/flywheel-hub/src/usage.rs`, `PRICES`): Anthropic first-party list prices as of 2026-10-06 (cache writes 1.25x input for 5 m TTL, 2x for 1 h; Haiku 5.5 / Sonnet 4.x long-context tiers; fast mode 2x where priced) and OpenAI GPT-5 prices (2025-11). A model not in it shows tokens only and `priced: false` with `unpriced_models` -- never a guessed price. Server-tool fees (web search) are not included.
- **Where**: each agent session's `usage` (`GET /api/sessions`, `fw hub ls [--json]`, events message `{"type":"usage"}`): turns, tokens, `cost_usd`, `today`, `week`, by model, last call. Header meter (`tokens · cost · billed model`; phone: cost; click or tap for the breakdown in a popover under it) and sidebar row show it (tooltip: breakdown). Costs are API list-price equivalents, also for subscription plans. `GET /api/usage` (also `usage` in `fw hub ls --json`): hub and per-workspace `today` / `week` (local Monday start) / `total`; the sidebar shows today and week.
- **Kept**: with the session's commands (`<state-dir>/commands/<id>.json`, 62 days of per-day totals, and where each earlier conversation's file was left, so `/resume` of one reads on instead of recounting); a removed session's usage moves to `<state-dir>/usage-archive.json` once (a removed id is never followed again) so rollups keep it.

## Suggested commands

Agents draw suggested commands with margins and wrapping, so copying them off the terminal breaks them. The hub reads the same text from the agent's own record instead and serves each fenced code block exactly as written.

- **Sources**: Claude Code's transcript (`projects/*/<agent_session_id>.jsonl`, or the hooks' `transcript_path`), assistant text only (not thinking, tool output, or subagent sidechains); Codex's rollout (`$CODEX_HOME` or `~/.codex`, `sessions/YYYY/MM/DD/rollout-*-<thread-id>.jsonl`, newest day first, at most 400 days back; restore's resume check uses the same search), or, when none is found, the `notify` payload's `last-assistant-message` (closed fences only: the report budget can cut it).
- **Tailing**: running agent sessions are checked every second (at once on a hook report) from a byte offset; lines over 8 MiB are skipped unread; a replaced or truncated file is re-read without duplicates; `/clear` (new conversation id) finishes the old file and follows the new one. The newest 500 blocks per session (and where reading stopped) are saved to `<state-dir>/commands/<session>.json`, so a restart keeps every block, pre-`/clear` ones included, under the same `seq`, and a resumed agent (restore) is followed on from where reading stopped. A session restored as a fallback shell is not followed (its saved blocks stay listed). A backlog read reports at most the newest 500. Codex rollout items carry no id: a block is keyed by its line's byte offset.
- **Shell blocks**: languages `bash sh zsh shell console shell-session fish ksh`, or untagged when the first non-comment line starts with a common command, a path (`./x`, `~/x`, `/x`), `*.sh`, `VAR=value`, or `$ `. `command` (what Copy/Send use) is `text`, except in `console`-style blocks (console languages, or untagged starting with `$ `): only the commands, `$ ` stripped, with continuations (after a trailing `\`, or `> ` lines, stripped) and the output lines left out.
- **Paste** (`Send`, `POST /sessions/{id}/paste`): the same encoding as `fw hub send` (`/input` with `paste`): bracketed when the target program enabled it (markers inside the text removed, newlines sent as CR), trailing newlines dropped, Enter (50 ms later, as its own keypress) only with `run`. A multi-line paste into a terminal without bracketed paste is refused (409) unless `run` (use Copy there).
- CLI: on a terminal, control characters in a block print as `^[` etc. (piped output and `--json` are exact). A shell session resolves to its workspace's single agent session. `--copy` writes OSC 52 to the terminal (stdout, else `/dev/tty`) and a confirmation to stderr: works over SSH in terminals that allow it; tmux needs `set -g set-clipboard on`; under GNU screen (`$STY`) it is DCS-wrapped; large copies (over ~75 KB encoded) are dropped by some terminals.

## Search

`Ctrl Shift F` (palette "Search sessions", the header's search pill on wide screens, the sidebar's search chip), `fw hub search`, or `GET /api/search`: one query across every session.

- **Sources**: `scrollback` -- every logical line the hub's emulator keeps (scrollback + screen, soft wraps joined; the primary screen also while a full-screen program is up, plus that program's screen), for every session with a terminal on this hub, exited ones included; `transcript` -- the current conversation of each agent session, running, stopped, or restored (Claude Code: user and assistant text and `tool_use` calls, not thinking, tool results, or subagent sidechains; Codex: user/assistant messages, function / custom tool / local shell calls, not its injected `<environment_context>`); `commands` -- the saved suggested code blocks.
- **Query**: literal (default) or `regex` (Rust `regex` syntax: linear time, no backtracking; `^`/`$` per line; compiled size capped at 1 MiB), case-insensitive unless `case`; at most 1 KiB. Empty matches are ignored; a match never spans two lines / messages / blocks.
- **Results**: grouped by session (sidebar order), each source newest first, one hit per line / message / block (`matches` counts the rest), a one-line snippet (160 characters before, ~320 in all, controls as spaces; the web cuts the context from the left so the match stays in view, the CLI keeps 60 before), `counts` = every matching unit. `per_session` hits per session and source (default 20, max 1000), `limit` in all (default 200, max 2000); API and CLI share these defaults. Past `limit` (`truncated`), the remaining sessions are still searched and listed with `counts` and no `hits`. Paging: `offset` skips each source's newest N hits (same query plus `session`, `offset` = hits seen; `more` false on the last page; the web's "more" button, CLI `--offset`). Terminal hits carry no time (the emulator does not keep one per line): `from_end` says how far up.
- **Bounds**: 1.5 s per request, then it answers with what it reached (`partial`); 2 searches at once (more wait); terminal text is extracted once per change of output; transcripts are indexed incrementally from a byte offset (lines over 8 MiB skipped, 64 KiB per message, 32 MiB per transcript counting text and message metadata, oldest dropped), re-read when replaced or truncated; all transcript indexes together hold at most 256 MiB (least recently searched dropped first) and one unsearched for 10 min is dropped (swept every transcript poll), so a dropped one is re-read on its next search. Measured (release, 20 sessions x 5k lines): ~30 ms after new output (text re-extracted), 0-6 ms otherwise; rerun with `cargo test -p flywheel-hub --release --test search twenty -- --nocapture`.
- **Machines** (web): with machines registered, one search asks this hub and every reachable machine in parallel through the proxy (`/m/<machine>/api/search`; a workspace or session filter asks only its machine), each machine with its own `limit` and a 6 s wait; results group under a head per machine (hits and time, or `offline` / `refused` / `no answer in 6 s`: not searched, which the status line repeats as partial). A machine's groups naming a session it does not own are dropped. A remote hit opens on its machine (attach, transcript, Commands drawer through the proxy). CLI: one hub per call, `fw hub search Q --machine NAME` through the local hub.
- **Opening a hit** (web): a terminal hit attaches the session, scrolls its terminal to the line and selects the match (xterm's search addon; a toast offers previous / next); a transcript hit opens the transcript viewer at that message (40-message pages, matches marked); a command hit opens the Commands drawer at that block. In the overlay: arrows pick, `Enter` opens (or searches, when typed since), `Alt R` regex, `Alt C` case, `Esc` closes; filters: workspace, session, kind, sources. One character searches only on `Enter`.
- What it reads is what the token holder can already read (terminals, the agent's own transcript files); auth as every route.

## Auth

- Token: 256 random bits, hex, `<state-dir>/token` (0600). Rotate with `fw hub token --rotate`.
- One-time codes: 20 Crockford base32 characters (100 bits), single use, 5 minutes, at most 64 outstanding (a 65th drops the oldest, so a QR still on screen can stop working), digests only in memory, dropped on rotation. Minted by `serve`, `url`, or `POST /api/login-codes`. Case, `-`/spaces and `i l o` look-alikes are forgiven when typed.
- Browsers: `/login?code=...` or `/login?token=...` (or the form at `/`, whose one field takes either) sets an `HttpOnly; SameSite=Strict` cookie named `fw_hub_<digest of the token>` (hubs on one host do not clobber each other's login) (`Secure` over the hub's own TLS, behind an `X-Forwarded-Proto: https` trusted proxy, or under an https `--public-url` / tailscale serve host). Its value is a fresh 256-bit per-login secret, never the token: digests in `<state-dir>/logins.json` (0600, survive restarts, 30 days, at most 256). `POST /logout` ends that login and closes the sockets it opened (code 4002; also on expiry or eviction); rotation ends all. The signed-in page replaces itself with `/` (keeping `#session=`), so Back never revisits a spent code URL.
- Scripts: `Authorization: Bearer <token>` (codes are not bearer credentials).
- Failure limit: 10 wrong credentials (token, code, cookie, or bearer) per client address in a sliding 10 minutes, then everything that client presents gets 429 + `Retry-After` until the oldest failure ages out -- except a live login cookie, so an attacker sharing a NAT/proxy address cannot lock signed-in users out. IPv6 clients are keyed by /64. Direct connections from this machine (loopback or the host's own address, no `X-Forwarded-For`: the CLI, a local browser) are exempt, so a remote attacker cannot lock out `fw hub url`. Caveat: a local tunnel that adds no `X-Forwarded-For` (`ssh -L`, socat) makes its traffic look direct, hence exempt.
- Share links (`fwview_...`, see Share links) reach only their own `/share/` path; anywhere else 403.
- Cross-origin `Origin` headers are refused on WebSocket upgrades and on every non-GET request.
- Connection bounds (`serve` raises its soft `RLIMIT_NOFILE` to the hard limit first):
  - **Authenticated** (the latest request carried the token, a login cookie, or a peer credential): at most a quarter of the descriptor limit (16-1024) at once (an upgraded WebSocket no longer counts); each request head within 10 s (also the keep-alive idle limit).
  - **Unauthenticated** (every connection until a request head on it carries a credential -- nothing is known before the head, so loopback CLI connections start here too, for microseconds): a separate pool of a quarter of that (8-128). The first head within 3 s, that request done within 10 s (public bodies: 4 KiB in 5 s), then `Connection: close`. A stop or `upgrade` drops them at once, never waits on them.
  - Clients: the TCP peer (IPv6: /64); behind loopback, a `--trusted-proxy`, or the host's own address, the `X-Forwarded-For` client once the head arrives (none: each connection its own). Nothing is refused at accept, so a flood sharing your address never locks out your token or cookie. A client past 8 connections loses its own oldest that has sent no head; past 8 requests without a credential, gets 429.
  - A full pool evicts from the client holding the most, silent connections before ones mid-request, oldest first; one evicted just as its head arrives may still authenticate. Limit: a flood from more addresses than the pool has slots can still reset strangers' logins (retry), never an authenticated request.

## Phone

Built for driving Claude Code and Codex from a phone (tested at 390x844 and 430x932 under iPhone 14 emulation):

- **Viewport**: the app is pinned to the visual viewport, so the on-screen keyboard shrinks the terminal instead of covering it (no page scroll or zoom on focus: fields are 16 px on touch screens); safe-area insets respected.
- **Row floor** (phones only): the PTY size is shared, so a phone never sizes it below 16 rows; a shorter terminal box (keyboard up, Compose open) is clipped around the cursor. Desktop panes fit their box exactly. With the keyboard up the quick answers shrink to one row and Compose hides the rest.
- **Needs you**: blocked and done agents of every workspace head the sidebar (the phone's switcher: drawer button, badge counts them) and are the phone's start screen (a phone opened without `#session=` lands there, never on a saved layout's pane); blocked first, longest-waiting first; worktree sessions show their branch (so do their browser notifications). Tap to open. Below 760 px the sidebar is a drawer; tablets get a narrower one.
- **Key bar** (touch screens; palette "Key bar: on/off/auto"), two rows. First, always in view: `esc` `tab` `shift-tab` (Claude's permission-mode cycle), sticky `ctrl` (tap: next key, twice: locked; applied to what the keyboard types), arrows (repeat when held; honour app cursor mode), `enter`, `^c`. Second, scrolls: sticky `alt`, `/ | ~ -`, `pgup` `pgdn`, text size; then `kbd` (show/hide the keyboard), `compose`. An armed modifier applies to every key-bar key as to a typed one (alt+`/` sends ESC `/`, ctrl+`-` ^_, ctrl+pgup CSI 5;5~). The first row's keys are 44px tall but share the width (32px each at 390px): a deliberate trade for keeping all ten in view.
- **Quick answers**: while the attached agent is blocked, the options of the prompt on the live screen (never the scrollback in view) become buttons: numbered lists (Claude's permission dialogs: sends the digit; an option ending `(y)` sends that key) or a `(y/n)` line (sends the letter + Enter); `esc` cancels. The status detail heads it.
- **Compose**: a multi-line text area (autocorrect, dictation) sent as one bracketed paste + Enter (`POST /paste` with `run`); `Paste` sends without Enter; history per device (arrows, or Up at the start); draft kept per session (switching sessions closes the panel; drafts of removed sessions are dropped); `Ctrl Enter` sends. Sign-out deletes history and drafts from the device.
- **Touch**: swipe scrolls the scrollback with momentum (in full-screen programs: wheel reports, else arrow keys); long-press selects a word, drag extends, then Copy / Copy screen; pinch or `A-` `A+` sets the text size (per device).
- **Alerts** (sidebar chip, per device): a system notification when an agent becomes blocked or done while the page is hidden, unfocused, or idle 5 min; each spell once, one per session and status per 15 s (a later spell waits for the window); clicking it opens that session. Needs https (`tailscale serve` or `--tls auto`) or localhost; on iOS only from the Home Screen app.
- **Install**: `/manifest.webmanifest` (standalone, flywheel icons) + `/sw.js`. The service worker never touches `/api`, sockets, or sign-in; it serves the cached signed-in page and `/assets/` only when the hub is unreachable (the page then says so and reconnects), is replaced with each hub build, and is emptied on sign-out. The Home Screen app has its own cookies on iOS: sign in once more there (`fw hub url --qr`).

## Flywheel panel

**Postgres-backed repos read the DSN from the hub's own environment** (`FLYWHEEL_PG_DSN`, else `DATABASE_URL`; flywheel.toml never carries one): start `fw hub serve` from a shell that exports it. One DSN serves every postgres folder; the store label (`postgres host:port/db`, no credentials) shows which database a panel read.

Workspace folders that are flywheel-adopted repos (a `flywheel.toml` or a `.flywheel/` dir) get a Flywheel panel: the verified loop beside the terminals. Read-only (a task file is written only by a fan-out's confirmed Promote); `--no-flywheel` turns it off (and Promote / fan out this task with it).

- **What it shows**: active phases (`tasks/active/<phase>/`) and their tasks as `pending | running | verified | failed | parked | awaiting | blocked` (from `fw status --json`: `verified` = a DONE lifecycle, `parked` = DONE but its landing parked); in-flight runs (the console's live rows); the 12 newest runs with their latest attempt's grader receipts; worker liveness (a live lease in `task_claims` -- an idle worker holds none) and autopilot liveness (`.flywheel/logs/autopilot/activity.json`); claims; and alerts (failed, stranded, stopped, awaiting a gate, blocked), which also join the sidebar's **Needs you** list: one row per alert while a folder has 1-2, else one row for the folder. **Mark seen** in the panel drops a folder's current alerts from Needs you in this browser until they change or new ones appear. Runs show DONE as `verified`.
- **Worktrees**: flywheel keeps a store per checkout (`fw worker` / `fw status` resolve `.flywheel/` under the worktree itself), so a workspace folder that is a linked git worktree of an adopted repo reads its own store (empty until a worker runs there) and its panel says `Linked worktree of <main>`; Start worker here runs there, against that store. A sibling-worktree session not in a watched folder gets its base folder's panel, labelled as that checkout's loop.
- **Where**: an adopted folder's sidebar chip gets a `verified/total` badge (plus the unseen alert count, red, pulsing while runs or daemons are live); click it, the `flywheel` header pill (attached session in an adopted folder; `Alt F`), a Needs-you row, or the palette.
- **Actions**: **Start worker here** opens a hub shell session `fw worker` (`fw worker 2`, ... when taken) running `<this fw> worker` in the folder, in a split below the focused pane (never over it; a new tab without room) (a real terminal; when it exits, Ctrl-C included, the shell stays; a restart restores it as a plain shell). While a worker the hub started still runs there, a start is refused (409) unless confirmed as another. **Output** on a task / run opens its `fw show` summary and its redacted agent transcript (`<store dir>/logs/runs/<run>.jsonl`, newest 1500 entries, followed live while the run is in flight). **fan out** on a task not yet verified launches a fan-out seeded from it (see [Fan-outs and flywheel tasks](#fan-outs-and-flywheel-tasks)).
- **Cost**: one watcher thread per adopted folder; every 2s it stats the policy, store (+ `-wal`), task tree, and activity record, and re-reads only on a change or every 15s (leases lapse with time). Each folder's own `flywheel.toml` is read (paths anchored at the folder, never the hub's cwd); stores are opened once and kept until the folder leaves every workspace (a failed read reconnects). A missing SQLite store is never created (tasks list as pending). Tracker sources (`github*`) are not listed (a network call per read): runs, claims, and alerts still show. Opening a store runs its idempotent schema bootstrap, as every `fw` read verb does.
- **Seam**: `flywheel-hub` defines `flywheel::FlywheelProvider` and plain data; the `flywheel` crate implements it (`hub_flywheel.rs`) over its console/status code and injects it into `fw hub serve`, so the hub never depends on the loop's crates.

## API

All under `/api`, JSON, token required. Errors are `{"error":"..."}` (unknown endpoints 404 and wrong methods 405 too); a malformed body is 400, a JSON body of the wrong shape 422, an unreadable query string 400 in the caller's words (`limit must be a number`). The pages (`/`, `/login`) answer an unreadable query string (and `POST /login` an unreadable or non-form body) with a 4xx page, or with `{"error"}` to `Accept: application/json`. `cols`/`rows` of 0 mean the default (120x32).

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
| GET | `/sessions/{id}/review[?base=]` | `{"session_id","session","agent","mergeable","dir","worktree","branch","head","base":{"name","source","sha","merge_base","is_branch"},"ahead","behind","upstream","remotes","commits","committed","staged","unstaged","untracked","conflicts","operation","rebasing"?}`; a set is `{"files":[{"path","old_path"?,"status","insertions","deletions","binary"}],"insertions","deletions","truncated"}` |
| GET | `/sessions/{id}/review/diff?scope=committed\|staged\|unstaged\|untracked&path=[&old_path=&offset=&base=]` | `{"path","scope","binary","lines","offset","next","total_lines","truncated"}` |
| POST | `/sessions/{id}/review/{commit,push,pr,rebase,abort,merge}` | `{"dry_run":true,...}` -> plan `{"action","summary","details","warnings","commands":[{"dir","program","args","display"}],"fingerprint","refusal","needs_rebase","defaults"?}`; `{"confirm":fingerprint,...}` runs -> `{"ok","summary","ran":[{"display","code","output"}],"conflicts","url"?,"head","worktree"?}`; 409 refused/stale/failed. Bodies: commit `message`; push `remote`; pr `title`,`body`,`base`,`draft` |
| POST | `/sessions/{id}/handoff/compose` | `{"source":{"kind":"selection","text","from"?:"terminal\|transcript"}\|{"kind":"transcript","index"}\|{"kind":"command","seq"}\|{"kind":"last_command"}\|{"kind":"diff","path"?,"old_path"?,"base"?},"max_bytes"?:256..98304,"cross_machine"?}` -> `{"from":{"session_id","session","kind","branch"?,"machine"?},"source","what","prompt","content_bytes","kept_bytes","truncated"}` (sends nothing) |
| POST | `/sessions/{id}/handoff` | `{"prompt","from"?,"source"?,"truncated"?}` -> `{"to","paste"}`: paste into agent session `id`, then Enter (400 a shell / the source itself; at most 128 KiB) |
| POST | `/sessions/{id}/review/reviewer` | `{"kind"?,"base"?,"dry_run"?}` -> `{"author","reviewer"?,"kind","reused","fallback"?,"flags","base","branch","prompt","diff_bytes","diff_truncated","dry_run"}`: 201 launched, 200 asked again / dry; 409 nothing to review |
| GET | `/sessions/{id}/review/findings` | `{"author","reviewer":{"id","name","kind","running","status"?},"reply"?,"timestamp"?,"blocks":[{"lang","shell","text","command"}],"prompt"?,"truncated"}` (409 no reviewer) |
| POST | `/sessions/{id}/review/findings/send` | `{"prompt"?}` (an edit) -> `{"to","paste"}`: the findings into the author |
| POST | `/sessions/{id}/review/ask` | `{"kind":"commit_message\|resolve_conflicts","dry_run"?}` -> `{"agent","prompt","paste"?}` |
| GET | `/worktrees/plan?folder=&task=&base=` | `{"dir","default_base","branches","plan":{"path","branch","base"}\|null,"error"}` (the form checks as you type) |
| POST | `/sessions/{id}/kill` | 202: SIGHUP the session (SIGKILL after 3s); record kept. 409 when nothing is left to kill |
| POST | `/sessions/{id}/relaunch` | relaunch an ended (`exited`/`stopped`) session in place: same id, name, command, folder; fresh PTY + scrollback. Body `{"cols"?,"rows"?}`. 409 while it runs. `/restart` is its old name |
| GET | `/sessions/{id}/screen[?source=visible\|recent&lines=N]` | rendered plain text (below) |
| GET | `/sessions/{id}/explain` | `{"session","kind","status"\|null,"source":{"kind":"hook\|manifest\|heuristic\|hub\|none",...},"source_label","screen":{"state","rule","detail","evidence":[{"row","text","matched"}],"fallback"}\|null,"manifest":{"file","version","origin","path"?,"rules":[{"id","state","matched","reason"}]},"problems":[{"path","error"}],"note"}` (see Detection manifests) |
| POST | `/sessions/{id}/input` | `{"data","paste"?,"enter"?}` -> `{"bytes","bracketed"}`: type into it (`paste`: as `fw hub send`). 409 when it has no running terminal or is not reading |
| GET | `/sessions/{id}/commands[?limit=N&all=true]` | suggested code blocks, newest first (shell only unless `all`; `limit` 1..500, default 50): `{"session_id","conversation","transcript","commands":[{"seq","conversation","message_id","timestamp","block_index","lang","shell","text","command","source"}]}` |
| POST | `/sessions/{id}/paste` | `{"text","run"?}` -> `{"bracketed","ran","bytes"}`: paste a suggested command (as `/input` with `paste`, plus: trailing newlines dropped, at most 1 MiB, whitespace-only is a 400); 409 when not running, not reading, or unsafe multi-line |
| GET | `/sessions/{id}/ws` | WebSocket attach |
| GET | `/search?q=[&regex=&case=&ws=&session=&kind=&source=&limit=&per_session=&offset=]` | `{"query","regex","case_sensitive","sources","groups":[{"session_id","session","workspace_id","workspace","kind","state","terminal","conversation","counts":{"scrollback","transcript","commands"},"more","hits":[hit]}],"hits","truncated","offset","partial","elapsed_ms","searched":{"sessions","lines","messages","commands"},"notes"}`; `kind`/`source` comma lists; every hit has `source`, `matches`, `before`, `match`, `after`, `line` (in its message/block), and: scrollback `screen` (`primary\|alternate`), `index`, `from_end`, `match_text`, `occurrence`; transcript `index`, `message_id`, `role`, `kind` (`text\|tool`), `tool`, `timestamp`; commands `seq`, `lang`, `shell`, `timestamp`. 400 bad query/regex/params, 404 unknown `ws`/`session` |
| GET | `/sessions/{id}/transcript[?around=N&before=N&after=N]` | `{"session_id","conversation","transcript","first","next","complete","messages":[{"index","message_id","role","kind","tool"?,"timestamp","truncated"?,"text"}],"error"?}`: messages `around-before..=around+after` (default 30 each, at most 200 in all; without `around` the newest `before`); `first` = oldest index kept; 400 for a shell |
| GET | `/events[?client=ID]` | hub-wide events WebSocket (below) |
| GET / POST / DELETE | `/shares[?session=]` | list `{"shares":[{"id","session_id","session","name","created","expires","viewers","last_used"}]}` / mint `{"session_id","expires_in"?\|"expires"?,"name"?}` -> 201 `{"share","session","credential","path"}` (shown once) / revoke all (of `session`) `{"revoked":[...]}` |
| DELETE | `/shares/{id or label}` | `{"revoked":[...]}`; open views close (4004) |
| POST | `/sessions/{id}/record/start` \| `/stop` | `{"recording":{"name","session_id","title","part","size","started","modified","cols","rows","active"},"started"?}`: 201 started, 200 already recording; 409 not running / not recording |
| GET | `/recordings` | `{"dir","limits":{"max_file_bytes","max_total_bytes"},"total_bytes","recordings":[recording]}` newest recording first, its parts in order |
| GET / DELETE | `/recordings/{name}` | the `.cast` (`application/x-asciicast`, attachment) / 204 (409 while active) |
| GET | `/kinds` | `{"kinds":[{"kind","available","program"}]}`: which session kinds this host can launch |
| POST | `/login-codes` | `{"code","expires_in":300}`: a one-time code for `/login?code=` |
| POST | `/token/rotate` | 204: rotate the token (see `fw hub token --rotate`) |
| GET | `/folders/check?path=` | always 200 (even without `path`): `{"ok":true,"path":canonical,"git"}` or `{"ok":false,"error"}` (the workspace form validates as you type); `/repos/check` is the old name |
| GET | `/notify` | `{"push":null}` or `{"push":{"host","events","debounce_secs","settle_secs","detail"}}` |
| POST | `/notify/test` | 204 sent; 409 push off; 502 `{"error"}` the webhook refused or was unreachable |
| GET | `/flywheel` | `{"enabled","folders":[summary]}`; summary `{"folder","generated_at","phases","counts":{state:n},"alerts":[{"kind","subject","detail","run_id"}],"live_runs","worker_live","autopilot_live","problem","worktree_of"}` (`worktree_of`: the main checkout when the folder is a linked worktree, else null) |
| GET | `/flywheel/panel?folder=` | the whole panel (`phases`, `live`, `runs`, `worker`, `autopilot`, `claims`, `alerts`, `source`, `store`, `worktree_of`, `problem`); 404 unless an adopted workspace folder |
| GET | `/flywheel/run?folder=&run=&since=N` | `{"run_id","task_id","status","active","summary","from","entries":[{"ts","kind","header","body","error"}],"next"}`; poll with `since=next`. 400 bad run id, 404 unknown run, 503 while an earlier read of it still waits on the store |
| GET / POST | `/fanouts` | `{"enabled","runner","fanouts":[summary]}` newest first (`"summary":true`: no grader output, worktree reads up to 30 s old) / launch `{"workspace_id","folder"?,"base"?,"name"?,"prompt","candidates":[{"kind","model"?,"permission_mode"?,"approval"?,"sandbox"?}],"graders":[CMD\|{"run","name"?}],"timeout_secs"?,"protect"?,"commit_instruction"?}` -> 201 view. View: `{"id","name","workspace_id","workspace","folder","repo" (deprecated mirror),"base","base_sha","graders","sent_prompt","winner","counts","warnings","leader","settled","graded_at","candidates":[{"label","session_id","kind","model","branch","path","phase","verdict","rank","why","grade","stale","cost_usd","tokens","pickable",...}]}` |
| GET / DELETE | `/fanouts/{id}` | the ranked board / forget it (204; sessions and worktrees stay) |
| POST | `/fanouts/{id}/{grade,fix,pick,rebase,cleanup}` | `{"candidate"}` (grade: 202); fix `"dry_run"?` -> `{"prompt","paste"?}`; pick / rebase `{"dry_run":true}` -> plan (pick: `{"refusal","needs_rebase","merge":plan,"losers"}`) then `{"confirm":fingerprint}`; cleanup `{"candidates"?,"remove_worktrees"?,"force"?,"dry_run"?}` -> `{"items":[{"candidate","path","action","done","refusal","error"}],"fanout"}`. 409 refused, 503 fan-outs off |
| POST | `/fanouts/{id}/promote` | `{"task_id"?,"phase"?,"goal"?,"reference"?,"dry_run":true}` -> `{"plan":{"path","relative","contents","task_id","phase","new_phase","phases","notes"},"fingerprint","reference","warnings"}`, then `{"confirm":fingerprint}` -> `{"written":{"path","relative","task_id","phase","listed"},"plan","warnings","fanout"}`. 400 invalid, 404 not a flywheel repo / panel off, 409 changed since the dry run or exists |
| GET | `/flywheel/task?folder=&task=` | the task as a fan-out seed `{"folder","task_id","phase","goal","prompt","digest","graders","held_out":[name],"not_run"}`; `POST /fanouts` with `"from_task"` seeds server-side |
| POST | `/flywheel/worker` | `{"folder","workspace_id"?,"another"?,"cols"?,"rows"?}` -> 201 the new `fw worker` session; 409 `{"error","running":{"id","name"}}` while a hub-started worker runs there and `another` is not true |

WebSocket: server sends **text** `{"type":"snapshot","cols","rows","alt_screen","bytes"}`, then **binary** frames (the first `bytes` bytes are the snapshot, then live output), and **text** `{"type":"exit","code","signal"}` / `{"type":"reset"}` (fell behind: clear, a screen-only snapshot follows) / `{"type":"size","cols","rows"}` (a viewer resized it; in order with the output) / `{"type":"error","message"}`. Clients reset, size their terminal to `cols`x`rows`, replay the snapshot, then fit and send `resize` (the snapshot starts with RIS anyway). Client sends `{"type":"input","data":"..."}`, `{"type":"resize","cols":N,"rows":N}`, `{"type":"theme","foreground","background","cursor":"#rrggbb","ansi":[16 x "#rrggbb"]}` (the colours colour queries answer with), or binary raw input. Token rotation closes every socket with code 4001 (even one whose upgrade raced the rotation); the UI then shows sign-in with a "token was rotated" notice. A live handoff closes every socket with 1012: reconnect (the session carries on). A `{"type":"input"}` refused mid-handoff answers `{"type":"error"}`.

Session views carry `agent` (`{"status","detail"?,"since"}`, agent kinds only), `agent_session_id`, `worktree`, `companion_of`, `reviewer` / `review_of` (a review pairing), `recording` (the file being written, while recording), `live.link_viewers` (share-link viewers, when any), and `git` (`{"branch","dirty","untracked","ahead","behind","against"?}` from the badge cache; absent until read or outside git).

Events WebSocket (`client`: 1-64 of `[A-Za-z0-9_-]`, else 400; omitted, the connection gets its own): server sends `{"type":"snapshot","sessions":{id:status},"workspaces":{id:rollup}}` on connect and after falling behind, `{"type":"status","session_id","status"|null,"workspace_id","workspace_status"}` per change (as this client sees it), and `{"type":"sessions"}` when sessions or workspaces changed (re-list), and `{"type":"commands","session_id","fresh","commands":[...]}` when an agent suggested code blocks (newest first; `fresh`: written in the last 2 minutes, not a re-read backlog), `{"type":"flywheel","folder","summary"|null}` when an adopted folder's panel changed (`null`: no longer watched), and `{"type":"flywheel_reset"}` after missing some (re-list `/api/flywheel`), `{"type":"layout","workspace_id","rev"}` when a layout was saved or pruned (re-read it), `{"type":"fanout","id"}` when a fan-out changed (re-read it). `{"type":"sessions"}` also follows a terminal socket attaching or leaving (`live.clients`); it is paced per client (at most one per 250 ms; one that comes sooner is sent when the interval is up, never dropped), and repeats queued together fold (`status` / `usage` per session, `fanout` per id: each carries or fetches the current state). Client sends `{"type":"view","session_id":id|null,"visible"?:[ids]}` (the focused pane's session and the other panes on screen; all count as viewed; nothing while hidden). Token rotation closes it with 4001 too.

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

**First run** (no workspace yet): the start screen offers the `*.code-workspace` files under the import root (Import per file; Browse... opens the full import), a folder path to make a workspace from (checked by the hub; git or plain), and the 30-second shortcut tour (six steps with the bindings in force, the palette's naming Share and Record; each step advances after 5 s, paused while the pointer or focus is on it; finishing or dismissing it turns `keyboard.tour` off, hub-wide; palette "Shortcut tour" replays it).

Defaults below; every one is rebindable in Settings > Keyboard.

| Key | |
|---|---|
| `Alt ,` | Settings |
| `Ctrl K` | command palette (fuzzy: sessions, actions, themes); again inside it sends a literal `^K` |
| `Alt N` / `Alt Shift N` | new session / new workspace |
| `Alt 1`..`9`, `Alt Up/Down` | jump to the nth / previous / next session with a terminal (numbers skip gone ones) |
| `Alt R`; `F2` or double-click on a sidebar row | rename session |
| `Alt W`; `Delete` on a focused sidebar row | close session (confirms while it runs) |
| `Alt C` / `Alt Shift C` | Commands drawer / copy the last suggested shell command |
| ``Alt ` `` | flip between an agent and its companion shell (opened on first use) |
| `Alt F` | Flywheel panel of the attached session's folder |
| `Alt G` | Review of the attached session (git) |
| `Ctrl Shift F` | Search every session (scrollback, transcripts, commands) |
| (unbound) | Verified fan-outs board / new verified fan-out (bind in Settings > Keyboard) |
| (unbound) | Share the attached session / start or stop recording it (`share.open`, `record.toggle`; bind in Settings > Keyboard) |

The **Commands drawer** (`suggested` in the header, `Alt C`, or the palette; it overlays the terminal so opening it never resizes the PTY for other viewers) lists the attached agent's code blocks newest first (from a shell session: the workspace's agent that suggested last, or the one picked), unwrapped in monospace, each with Copy (the clipboard API on https/localhost, a selected-textarea fallback on plain-http LAN and iOS Safari), Send (paste into the chosen running shell of the workspace, no Enter) and Send + Run. "send to" defaults to the agent's companion shell, else the attached shell, else a shell in the agent's directory; with none, it is "new shell (companion)" and the buttons read Open shell & send / Open shell & run (start the companion, wait for its prompt, paste). A line under the pickers says where Send goes. Keys in the drawer: arrows / `j` `k` pick, `Enter` or `c` copy, `1`-`9` copy the card with that number (cards are numbered by position; the block's id is the tooltip), `s` Send, `Shift S` Send + Run, `Esc` close. Lines wider than the card fade at the edge with a `wide: scroll` hint. A new shell command from an agent in the workspace in view raises a toast with Copy. The header badge counts the drawer's source only: the attached agent's own unseen blocks (from a shell, the agent that suggested last, named beside the count).

Hub shortcuts are captured before the terminal sees them. On macOS the chord is `Ctrl Option` instead of `Alt`, so Option keeps typing layout characters (`| [ ] { }`, dead keys); elsewhere an `Alt` key the layout turns into a character also goes to the terminal. Served from the binary (no build step): xterm.js 6.0.0 + fit + web-links + search (@xterm/addon-search 0.16.0) and the UI fonts (Big Shoulders Display, Martian Mono, JetBrains Mono; SIL OFL), vendored under `crates/flywheel-hub/web/vendor/` with their licenses. Only the fonts, icons, `login.js`, `/manifest.webmanifest`, and `/sw.js` are served without the token (a share link serves its viewer's own scripts under its path) (the login page and installers use them; none holds app data).

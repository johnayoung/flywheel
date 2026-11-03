# Hub reference

Every `fw hub` verb and flag, the serve flags, and the hub's files. `fw hub <verb> --help` is authoritative; `crates/flywheel-hub/src/docs_drift.rs` fails the build when this page drifts from the CLI or `hub.toml`. Front door: [hub](../hub.md).

## Conventions

- `SESSION` / `WS`: id, unique id prefix, or name; an exact name beats a prefix; ambiguity is an error.
- Every verb but `report` takes `--state-dir DIR` (default `$XDG_STATE_HOME/flywheel/hub`, else `~/.local/state/flywheel/hub`).
- `{remote}` = `[--url URL] [--token T] [--fingerprint SHA256]`: a hub elsewhere. Prefer `FW_HUB_TOKEN` to `--token` (argv is world-readable). TLS trust: [hub-security](security.md#https-without-tailscale).
- `{machine}` = `[--machine NAME]`: a machine registered on that hub, reached through its proxy (`fw hub ls --machine desktop`).
- `{serve}` = the [serve flags](#serve-flags).

## Verbs

Server and lifecycle:

```bash
fw hub start {serve} [--timeout SECS] [--handoff]   # detached; returns once serving (30 s default), prints login URLs
fw hub stop [--timeout SECS] [--force]              # graceful (30 s); --force SIGKILLs after it (no restore record)
fw hub upgrade [--binary PATH] [--allow-restart] [--timeout SECS]   # live handoff, no session restarts; exit 0 done, 4 refused, 3 no hub, 1 failed
fw hub status [--json]                              # pid, uptime, url, counts; exit 0 running, 3 not, 1 unreachable
fw hub logs [--follow|-f] [--lines|-n N]            # <state-dir>/hub.log (detached hubs); -n 0 whole file, default 100
fw hub serve {serve} [--handoff]                    # foreground; --handoff takes over the running hub (--bind/--port ignored)
fw hub install-service {serve} [--name N] [--force] [--print]   # systemd --user unit NAME.service (default flywheel-hub); never enabled
fw hub doctor [--port N] [--json]                   # read-only machine check, a fix per finding; exit 1 if any fails
```

Sign-in, access, other hubs:

```bash
fw hub open [--session S] {remote} {machine}        # browser here, signed in (fresh one-time code; URL printed too)
fw hub url [--qr] {remote}                          # one-time login URLs, most remote-friendly first; --qr for a phone
fw hub token [--rotate]                             # print the token (made on first use) / replace it
fw hub tls ca [--out FILE] [--der]                  # local CA: fingerprint, constraints, file; --out writes a copy (PEM, DER with --der)
fw hub tls rotate                                   # new CA, old key deleted; every device re-installs
fw hub peer issue --name X [--json]                 # a credential another hub holds to reach this one (printed once)
fw hub peer ls [--json]
fw hub peer revoke NAME|ID
fw hub machine add NAME URL [--mint] [--peer-name N] [--fingerprint SHA256] [--replace] [--json]   # secret from FW_HUB_MACHINE_SECRET or stdin
fw hub machine ls [--no-probe] [--json]
fw hub machine rm NAME [--json]                     # also asks the machine to revoke the credential
fw hub machine test [NAME] [--json]
fw hub share SESSION [--expires D] [--name LABEL] [--qr] [--json] {remote}   # read-only live view link
fw hub share ls [--json] {remote}
fw hub share revoke [ID|LABEL] [--all] [--session S] {remote}
```

Sessions:

```bash
fw hub ls [--json] {remote} {machine}               # workspaces > sessions: id, kind, state (+ agent status), name, cwd [branch +ahead -behind ~changed ?untracked]
fw hub new WS [--kind shell|claude|codex] [--folder PATH|LABEL] [--name N] [--create] [--worktree TASK] [--base REF] [--json] {remote} {machine} [-- ARGV...]
    [--prompt TEXT|-] [--model M] [--permission-mode P] [--approval A] [--sandbox S] [--template NAME]   # agents; see workflows: Agent launcher
fw hub template ls [WS] [--json] {remote} {machine}
fw hub template save WS NAME [--kind claude|codex] [--prompt TEXT|-] [--model M] [--permission-mode P] [--approval A] [--sandbox S] [--worktree] [--no-worktree] [--replace] [--edit] {remote} {machine}
fw hub template rm WS NAME {remote} {machine}
fw hub send SESSION TEXT|- [--enter] [--raw] {remote} {machine}       # pasted: newlines -> CR, bracketed when enabled
fw hub read SESSION [--source visible|recent] [--lines N] [--json] {remote} {machine}
fw hub wait SESSION --until done|blocked|idle|working|exit [--timeout SECS] [--changed] {remote} {machine}   # exit 0 met, 124 timeout, 1 error
fw hub attach [SESSION] [--prefix C-b] [--no-status] {remote} {machine}   # picker without SESSION
fw hub kill SESSION [--rm] [--remove-worktree] [--force] {remote} {machine}   # hang up; --rm forgets; --remove-worktree needs --rm
fw hub relaunch SESSION {remote} {machine}          # an exited/stopped session again in place
fw hub explain [SESSION] [--file SCREEN.txt|-] [--kind claude|codex|shell] [--json] {remote} {machine}   # why its status; --file: a saved screen, no hub
fw hub import FILE.code-workspace... [--name N] [--update] [--json]   # VS Code multi-root workspaces -> workspaces
```

Agent output:

```bash
fw hub commands SESSION [--last] [--all] [--json] [--copy] [--limit N] {remote} {machine}   # suggested code blocks, newest first (default 20)
fw hub search QUERY [--regex] [--case] [--session S] [--workspace W] [--kind K]... [--source scrollback|transcript|commands]... [--limit N] [--per-session N] [--offset N] [--json] {remote} {machine}   # exit 0 hits, 1 none
fw hub record start SESSION [--json] {remote}
fw hub record stop SESSION [--json] {remote}
fw hub record ls [--json] {remote}
fw hub record get NAME [--output|-o FILE|-] [--force] {remote}
fw hub record rm NAME {remote}
```

Git and agents together:

```bash
fw hub review SESSION [--base REF] [--diff PATH] [--scope committed|staged|unstaged|untracked] [--json] {remote} {machine}
fw hub merge SESSION [--yes|-y] [--rebase] [--remove-worktree] [--abort-rebase] [--json] {remote} {machine}   # ff the base onto a worktree branch
fw hub handoff FROM TO (--selection-file F|- | --diff [--path P] [--base REF] | --last-command | --command SEQ | --message N) [--to-machine M] [--max-bytes N] [--edit] [--dry-run] [--json] {remote} {machine}
fw hub ask-review SESSION [--kind claude|codex] [--base REF] [--dry-run] [--findings] [--send-findings] [--json] {remote} {machine}
fw hub fanout new WS (--prompt TEXT|- --grader [name=]CMD... | --from-task ID) --agent KIND[:MODEL]... [--folder F] [--base BRANCH] [--name N] [--timeout SECS] [--protect PATH]... [--permission-mode M] [--approval A] [--sandbox S] [--no-commit-instruction] [--json] {remote} {machine}
fw hub fanout ls [--json] {remote} {machine}
fw hub fanout show FANOUT [--verbose|-v] [--json] {remote} {machine}
fw hub fanout wait FANOUT [--timeout SECS] [--json] {remote} {machine}
fw hub fanout grade FANOUT CAND {remote} {machine}
fw hub fanout fix FANOUT CAND [--yes|-y] [--json] {remote} {machine}
fw hub fanout pick FANOUT CAND [--yes|-y] [--rebase] [--json] {remote} {machine}
fw hub fanout promote FANOUT [--task-id ID] [--phase NAME] [--goal TEXT] [--reference CAND|none] [--yes|-y] [--json] {remote} {machine}
fw hub fanout cleanup FANOUT [CAND...] [--stop-only] [--force] [--yes|-y] [--json] {remote} {machine}
fw hub fanout rm FANOUT {remote} {machine}          # forget the record; sessions and worktrees stay
```

Settings and hooks:

```bash
fw hub config check [--file PATH]                   # validate hub.toml; exit 1 invalid
fw hub config show [--json]                         # every key: value, source (flag|file|default), when it applies
fw hub plugin ls [--json]
fw hub plugin enable NAME
fw hub plugin disable NAME
fw hub plugin logs NAME [--lines|-n N] [--json]     # newest runs (default 20)
fw hub report --source claude|codex --event E [--detail D] [--stdin] [PAYLOAD]   # agent status hooks only
```

Prompt queues ([hub-workflows: Prompt queue](workflows.md#prompt-queue)); `ITEM` = a position (1 = next) or an id prefix:

```bash
fw hub queue add SESSION TEXT|- [--when done|idle|blocked-cleared] [--json] {remote} {machine}   # typed in once the turn is done
fw hub queue ls [SESSION] [--json] {remote} {machine}   # no SESSION: each one's latest 3 sent too
fw hub queue rm SESSION ITEM [--json] {remote} {machine}
fw hub queue clear SESSION [--all] [--json] {remote} {machine}   # --all: the delivered history too
fw hub queue move SESSION ITEM POS [--json] {remote} {machine}   # POS among the pending (1 = next)
fw hub queue pause SESSION [--json] {remote} {machine}
fw hub queue resume SESSION [--json] {remote} {machine}   # also clears a stall
```

### Verb notes

- **`new`**: `--create` makes workspace `WS` (with `--folder`) if none matches. `--folder` (alias `--repo`) is a path or a folder's label / dir name. Shell `ARGV` replaces `$SHELL`.
- **`send`**: bracketed paste only when the program enabled it; a final newline becomes Enter, sent 50 ms later. `--raw` sends bytes untouched (`fw hub send s $'\x03' --raw` is Ctrl-C).
- **`import`**: VS Code `*.code-workspace` files (JSONC); each `folders` entry becomes a folder, its `name` the label. Missing and non-local entries are skipped and listed. `--update` replaces an existing workspace's folders.
- **`open`**: mints a one-time code and opens it via a 0600 redirect page in the state dir (the code never goes on argv). Opener: `$BROWSER`, else `wslview`, `cmd.exe /c start`, `xdg-open` / `open`.
- **`url`**: a fresh code under every URL that reaches the hub, most remote-friendly first (public, tailnet https, tailnet, lan, this machine). `--qr` draws the first remote one. The token is never printed.
- **`report`**: the agent hook entry point; needs the session's env, never prints to stdout, fails with exit 1 (never 2, which would stop a Claude `Stop`).
- Semantics of the rest: [hub-workflows](workflows.md) (`wait`, `kill`, `review`, `merge`, `handoff`, `fanout`) and [hub-internals](internals.md) (`attach`, `explain`, `upgrade`, `doctor`).

## Serve flags

Taken by `serve`, `start`, `install-service`. All but `--notify-url` are also `hub.toml` keys; a flag wins over the file.

| Flag | Default | |
|---|---|---|
| `--bind` | `127.0.0.1` | non-loopback prints a warning |
| `--port` | `7420` | `0` picks a free port |
| `--public-url` | none | origin browsers reach the hub at through a proxy (no path); repeatable; listed first by `url` |
| `--allow-origin` | none | extra browser origin accepted; repeatable; same-origin always allowed |
| `--trusted-proxy` | none | proxy on another machine whose `X-Forwarded-*` are believed (loopback always is); repeatable |
| `--tls` | `off` | `auto` (local name-constrained CA), `off`, or `CERT,KEY` (PEM files you supply) |
| `--tls-name` | none | extra name, IP, or network (at most /8) for the local CA; repeatable; later additions need `fw hub tls rotate` |
| `--scrollback-lines` | `5000` | per session, capped at 1M cells; 24 B a cell (~14 MiB for 5000 x 120) |
| `--no-restore` | off | drop pending restores for good |
| `--no-agent-resume` | off | restore agents as fresh shells |
| `--no-flywheel` | off | no Flywheel panel (nor Promote / fan out this task) |
| `--import-root` | `~/code` | where the UI's VS Code import looks (3 levels deep) |
| `--notify-url` | none | ntfy-compatible push URL; overrides `url` in `notify.toml` (visible in `ps`: prefer the file) |
| `--worktree-branch-prefix` | none | session worktree branches are `<prefix><task>` (e.g. `agent/`) |
| `--record-part-cap` | `64MiB` | a recording part past it rotates (`64KiB`-`4GiB`; bytes or `KiB`/`MiB`/`GiB`) |
| `--record-dir-cap` | `1GiB` | recordings dir cap, oldest finished deleted first (>= part cap, <= `1TiB`) |
| `--share-default-expiry` | `1h` | a share link naming no expiry (<= max) |
| `--share-max-expiry` | `30d` | longest share expiry accepted (`60s`-`30d`) |

## hub.toml

`<state-dir>/hub.toml`. Precedence: **flag > file > default** (a list flag replaces the list). Unknown keys and bad values are errors naming the key; `serve` refuses an invalid file (exit 2).

- Edited while running: re-read within 1 s. Live keys apply at once; `fw hub config show` lists the rest as pending restart.
- An invalid edit keeps the last good settings and is reported in the log, a UI toast, and `fw hub config check` (exit 1).

```toml
[server]                          # restart to apply
bind = "127.0.0.1"
port = 7420
public_urls = ["https://box.tailnet.ts.net"]
allow_origins = []
trusted_proxies = []
tls = "off"                       # auto: local name-constrained CA; or "/path/cert.pem,/path/key.pem"
tls_names = []                    # extra names/IPs/networks for it, e.g. ["192.168.0.0/16"]

[sessions]
scrollback_lines = 5000           # live: sessions started from now on
restore = true                    # restart
agent_resume = true               # restart

[worktrees]
branch_prefix = "agent/"          # live

[import]
root = "~/code"                   # live

[flywheel]
enabled = true                    # restart

[agents.claude]                   # live: a launch naming none gets these
model = "opus"
permission_mode = "acceptEdits"
[agents.codex]
model = "gpt-5-codex"
approval = "on-request"
sandbox = "workspace-write"

[plugins]
enabled = true                    # live: off runs no plugin
max_concurrent = 4                # restart

[recordings]                      # live: running recordings too
part_cap = "64MiB"                # a part past it rotates (64KiB..4GiB)
dir_cap = "1GiB"                  # oldest finished recordings go past it

[shares]                          # live: links made from now on
default_expiry = "1h"             # a link naming no expiry
max_expiry = "30d"                # the longest one may name (60s..30d)

[review]                          # live: "ask for a review"
reviewer = "other"                # other (claude <-> codex) | same | claude | codex

[queue]                           # live: prompt queues
typing_quiet = "8s"               # no typing in a session this long before its queue delivers (1s..10m)
```

| Key | Flag | Default | Applies |
|---|---|---|---|
| `server.bind` | `--bind` | `127.0.0.1` | restart |
| `server.port` | `--port` | `7420` | restart |
| `server.public_urls` | `--public-url` | `[]` | restart |
| `server.allow_origins` | `--allow-origin` | `[]` | restart |
| `server.trusted_proxies` | `--trusted-proxy` | `[]` | restart |
| `server.tls` | `--tls` | `"off"` (CERT,KEY paths absolute) | restart |
| `server.tls_names` | `--tls-name` | `[]` (no single-label names) | restart |
| `sessions.scrollback_lines` | `--scrollback-lines` | `5000` (sessions started from now on) | live |
| `sessions.restore` | `--no-restore` | `true` | restart |
| `sessions.agent_resume` | `--no-agent-resume` | `true` | restart |
| `worktrees.branch_prefix` | `--worktree-branch-prefix` | `""` | live |
| `import.root` | `--import-root` | `"~/code"` | live |
| `flywheel.enabled` | `--no-flywheel` | `true` | restart |
| `agents.claude.model` | - | none (fills a launch that names none) | live |
| `agents.claude.permission_mode` | - | none | live |
| `agents.codex.model` | - | none | live |
| `agents.codex.approval` | - | none | live |
| `agents.codex.sandbox` | - | none | live |
| `plugins.enabled` | - | `true` (false runs no plugin) | live |
| `plugins.max_concurrent` | - | `4` (1-16) | restart |
| `recordings.part_cap` | `--record-part-cap` | `"64MiB"` (next event of running recordings too) | live |
| `recordings.dir_cap` | `--record-dir-cap` | `"1GiB"` | live |
| `shares.default_expiry` | `--share-default-expiry` | `"1h"` (links made from now on) | live |
| `shares.max_expiry` | `--share-max-expiry` | `"30d"` | live |
| `review.reviewer` | - | `"other"` | live |
| `queue.typing_quiet` | - | `"8s"` (1s-10m) | live |

Not in hub.toml: push (`notify.toml`, it holds secrets), detection overrides (`agent-detection/`), plugins (`plugins/`), UI settings (`settings.json`).

## notify.toml

`<state-dir>/notify.toml`, must be `chmod 600`. Behaviour: [hub-internals](internals.md#push-notifications).

```toml
url = "https://ntfy.example.com/hub"
token = "tk_..."               # optional, sent as Authorization: Bearer (or user:pass@ in the URL: Basic)
events = ["blocked", "done"]   # default both
debounce_secs = 30             # one push per session and status per window (a new spell inside it waits for its end)
settle_secs = 3                # how long a status must hold before it pushes (max 60)
detail = false                 # names only: leave out what a blocked agent asks (default true)
```

## plugin.toml

**A plugin runs as you, unsandboxed; the hub reviews nothing.** Install by copying a directory to `<state-dir>/plugins/<name>/`; the UI and API never download one. New plugins are **disabled** until `fw hub plugin enable NAME`. Re-scanned every 2 s. Sample: `crates/flywheel-hub/plugins/hello`. Run protocol: [hub-api](api.md#plugins).

```toml
description = "Ping me when an agent is stuck"
command = ["./hook.sh"]            # argv, no shell; ./x is the plugin dir's, a bare name is looked up on PATH
timeout_secs = 10                  # 1-300
[[events]]
on = "status.changed"              # any event in api.md#plugin-events
statuses = ["blocked", "done"]     # optional (status.changed): working blocked done idle unknown
kinds = ["claude", "codex"]        # optional: sessions of these kinds (never matches fanout.finished)
[[actions]]                        # palette "Plugin: <title>", Settings > Plugins
id = "summarize"
title = "Summarize this session"
context = "session"                # needs an attached session (default "hub")
command = ["./summarize.py"]       # optional: else the top-level command
```

## UI settings

`Alt ,` or the sidebar gear. Stored by the hub (`settings.json`), so they follow you to every device; any one can be kept for **this device** only.

| Section | |
|---|---|
| Appearance | theme (auto / dark / light), terminal font (JetBrains Mono / Martian Mono / system), text size (unset: 13, phones 11), density |
| Terminal | scrollback in view (client side; the hub's is `sessions.scrollback_lines`), cursor style / blink, ligatures in transcripts, commands, run output, diffs (off) |
| Keyboard | pane prefix, key bar, companion key mode, shortcut tour; every shortcut, rebindable |
| Notifications | system notifications (device only), command toasts |
| Agents | the new-session form's pre-filled model / permission mode / approvals / sandbox per kind |
| Plugins | per plugin: enable toggle, events, actions (run), last run, last error, logs |
| Server | read-only: effective hub.toml with sources, file error, pending restart |

**Key bindings**: up to 4 per action, `mod+...+key` (`mod` = Ctrl, Cmd on macOS). The server refuses (422) keys the browser or OS keeps (`ctrl+w`, `ctrl+t`, `alt+tab`, any `meta+`), keys terminals need (no modifier, `ctrl+c`, `ctrl+d`, ...), and duplicates in one scope. It warns on keys shells lose (`alt+b/f`, `ctrl+<letter>`) and keys desktops take (`ctrl+alt+t`).

## Keyboard shortcuts

Defaults, all rebindable (Settings > Keyboard). `mod` = Ctrl (Cmd on macOS); `alt` = Alt (Ctrl Option on macOS). `prefix.*` keys work only after `prefix.arm` (pressed twice it sends `^B`).

| Action | Default | |
|---|---|---|
| `palette.open` | `mod+k` | Command palette (pressed again inside it: the key goes to the terminal) |
| `search.open` | `mod+shift+f` | Search every session |
| `settings.open` | `alt+,` | Settings |
| `keys.show` | - | Keyboard shortcuts |
| `session.new` | `alt+n` | New session |
| `workspace.new` | `alt+shift+n` | New workspace |
| `session.jump` | `alt+1..9` | Jump to the nth session |
| `session.prev` | `alt+up` | Previous session |
| `session.next` | `alt+down` | Next session |
| `session.rename` | `alt+r` | Rename the attached session |
| `session.close` | `alt+w` | Close the attached session |
| `share.open` | - | Share the attached session (read-only link) |
| `record.toggle` | - | Start or stop recording the attached session |
| `commands.toggle` | `alt+c` | Suggested commands drawer |
| `commands.copy_last` | `alt+shift+c` | Copy the last suggested shell command |
| `companion.flip` | `` alt+` `` | Flip between an agent and its companion shell |
| `flywheel.toggle` | `alt+f` | Flywheel panel |
| `review.toggle` | `alt+g` | Review the attached session's git work |
| `handoff.send` | - | Send the terminal selection to another agent |
| `review.ask` | - | Ask an agent of the other kind to review the attached session |
| `queue.toggle` | `alt+q` | Prompt queue of the attached agent |
| `fanout.open` | - | Verified fan-outs |
| `fanout.new` | - | New verified fan-out |
| `pane.split_right` | `alt+\` | Split right with a new shell |
| `pane.split_down` | `alt+-` | Split down with a new shell |
| `pane.focus_left` | `alt+shift+left` | Focus the pane to the left |
| `pane.focus_down` | `alt+shift+down` | Focus the pane below |
| `pane.focus_up` | `alt+shift+up` | Focus the pane above |
| `pane.focus_right` | `alt+shift+right` | Focus the pane to the right |
| `pane.resize_left` | `alt+shift+h` | Move the nearest divider left |
| `pane.resize_down` | `alt+shift+j` | Move the nearest divider down |
| `pane.resize_up` | `alt+shift+k` | Move the nearest divider up |
| `pane.resize_right` | `alt+shift+l` | Move the nearest divider right |
| `pane.zoom` | `alt+z` | Zoom the focused pane |
| `pane.close` | `alt+x` | Close the focused pane |
| `tab.new` | `alt+t` | New tab |
| `tab.prev` | `alt+pageup` | Previous tab |
| `tab.next` | `alt+pagedown` | Next tab |
| `prefix.arm` | `ctrl+b` | The pane prefix key (with the prefix on) |
| `prefix.split_right` | `%` | Split right |
| `prefix.split_down` | `"` | Split down |
| `prefix.focus_left` | `left` | Focus left |
| `prefix.focus_down` | `down` | Focus down |
| `prefix.focus_up` | `up` | Focus up |
| `prefix.focus_right` | `right` | Focus right |
| `prefix.resize_left` | `ctrl+left` | Move the divider left |
| `prefix.resize_down` | `ctrl+down` | Move the divider down |
| `prefix.resize_up` | `ctrl+up` | Move the divider up |
| `prefix.resize_right` | `ctrl+right` | Move the divider right |
| `prefix.zoom` | `z` | Zoom |
| `prefix.close_pane` | `x` | Close the pane |
| `prefix.new_tab` | `c` | New tab |
| `prefix.next_tab` | `n` | Next tab |
| `prefix.prev_tab` | `p` | Previous tab |
| `prefix.tab` | `1..9` | The nth tab |
| `prefix.rename_tab` | `,` | Rename the tab |
| `prefix.close_tab` | `&` | Close the tab |
| `prefix.next_pane` | `o` | Next pane |
| `prefix.help` | `?` | Keyboard shortcuts |

Also: `F2` / double-click renames and `Delete` closes on a focused sidebar row; `Ctrl K` again inside the palette sends `^K`.

## Environment

| Variable | |
|---|---|
| `FW_HUB_TOKEN` | token for `{remote}` verbs |
| `FW_HUB_MACHINE_SECRET` | `machine add` secret (else stdin) |
| `FLYWHEEL_PG_DSN`, `DATABASE_URL` | the Flywheel panel's Postgres DSN, read from the hub's own environment |
| `BROWSER` | `fw hub open`'s opener |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | where transcripts / rollouts are read (default `~/.claude`, `~/.codex`) |
| set in sessions | `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`; agents also `FLYWHEEL_HUB_REPORT_SOCKET`, `FLYWHEEL_HUB_REPORT_SECRET` |

## State dir

All files 0600, the dir 0700.

| Path | |
|---|---|
| `token` | access token (256 bits, hex) |
| `logins.json` | login-cookie digests (30 days, at most 256) |
| `state.json` | workspaces, sessions, layouts, templates, fan-outs, prompt queues (schema 5; older migrate, newer refused) |
| `settings.json` | UI settings |
| `hub.toml`, `notify.toml` | above |
| `server.json`, `serve.lock`, `report.sock`, `handoff.sock` | running hub: pid/address/URLs, one-hub lock, hook channel, live-handoff socket |
| `hub.log`, `hub.log.1..3` | detached hub log (rotated at 10 MiB) |
| `machines.json`, `peers.json`, `shares.json` | registered machines (plaintext credentials), issued peer digests, share-link digests |
| `commands/<id>.json`, `usage-archive.json` | suggested blocks + usage per session; removed sessions' usage |
| `recordings/`, `plugins/`, `plugins.json`, `plugin-logs/`, `agent-detection/`, `tls/` | recordings, plugins and their enablement and logs, detection overrides, local CA |
| `open-<id>.html` | `fw hub open` redirect pages (swept when expired) |

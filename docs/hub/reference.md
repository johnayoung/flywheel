# Hub reference

Every `fw hub` verb and flag, the serve flags, the hub's files. `--help` is authoritative; `crates/flywheel-hub/src/docs_drift.rs` fails the build when this page drifts from the CLI or `hub.toml`. Front door: [hub](../hub.md).

## Conventions

- `SESSION` / `WS`: id, unique id prefix, or name (an exact name beats a prefix; ambiguity errors).
- Every verb but `report` takes `--state-dir DIR` (default `$XDG_STATE_HOME/flywheel/hub`, else `~/.local/state/flywheel/hub`).
- `{remote}` = `[--url URL] [--token T] [--fingerprint SHA256]`: a hub elsewhere; prefer `FW_HUB_TOKEN` (argv is world-readable). TLS trust: [hub-security](security.md#https-without-tailscale).
- `{machine}` = `[--machine NAME]`: a machine registered on that hub, via its proxy.
- `{serve}` = the [serve flags](#serve-flags).

## Verbs

Server and lifecycle:

```bash
fw hub start {serve} [--timeout SECS] [--handoff]   # detached; returns once serving, prints login URLs
fw hub stop [--timeout SECS] [--force]              # graceful; --force SIGKILLs after --timeout
fw hub upgrade [--binary PATH] [--allow-restart] [--timeout SECS]   # live handoff; exit 0 done, 4 refused, 3 no hub, 1 failed
fw hub status [--json]                              # exit 0 running, 3 not, 1 unreachable
fw hub logs [--follow|-f] [--lines|-n N]            # hub.log; -n 0 whole file, default 100
fw hub serve {serve} [--handoff]                    # foreground; --handoff takes over the running hub
fw hub install-service {serve} [--name N] [--force] [--print]   # systemd --user unit (default flywheel-hub), not enabled
fw hub doctor [--port N] [--json]                   # read-only check, a fix per finding; exit 1 on a FAIL
```

Sign-in, access, other hubs:

```bash
fw hub open [--session S] {remote} {machine}
fw hub url [--qr] {remote}                          # one-time login URLs; --qr for a phone
fw hub token [--rotate]
fw hub tls ca [--out FILE] [--der]                  # local CA fingerprint, constraints; --out a copy
fw hub tls rotate                                   # new CA; every device re-installs
fw hub peer issue --name X [--json]
fw hub peer ls [--json]
fw hub peer revoke NAME|ID
fw hub machine add NAME URL [--mint] [--peer-name N] [--fingerprint SHA256] [--replace] [--json]   # secret: FW_HUB_MACHINE_SECRET or stdin
fw hub machine ls [--no-probe] [--json]
fw hub machine rm NAME [--json]
fw hub machine test [NAME] [--json]
fw hub share SESSION [--expires D] [--name LABEL] [--qr] [--json] {remote}
fw hub share ls [--json] {remote}
fw hub share revoke [ID|LABEL] [--all] [--session S] {remote}
```

Sessions:

```bash
fw hub ls [--json] {remote} {machine}
fw hub new WS [--kind shell|claude|codex] [--folder PATH|LABEL] [--name N] [--create] [--worktree TASK] [--base REF] [--json] {remote} {machine} [-- ARGV...]
    [--prompt TEXT|-] [--model M] [--permission-mode P] [--approval A] [--sandbox S] [--template NAME]
fw hub template ls [WS] [--json] {remote} {machine}
fw hub template save WS NAME [--kind claude|codex] [--prompt TEXT|-] [--model M] [--permission-mode P] [--approval A] [--sandbox S] [--worktree] [--no-worktree] [--replace] [--edit] {remote} {machine}
fw hub template rm WS NAME {remote} {machine}
fw hub send SESSION TEXT|- [--enter] [--raw] {remote} {machine}
fw hub read SESSION [--source visible|recent] [--lines N] [--json] {remote} {machine}
fw hub wait SESSION --until done|blocked|idle|working|exit [--timeout SECS] [--changed] {remote} {machine}   # 0 met, 124 timeout, 1 error
fw hub attach [SESSION] [--prefix C-b] [--no-status] {remote} {machine}
fw hub kill SESSION [--rm] [--remove-worktree] [--force] {remote} {machine}   # --rm forgets; --remove-worktree needs --rm
fw hub relaunch SESSION {remote} {machine}
fw hub explain [SESSION] [--file SCREEN.txt|-] [--kind claude|codex|shell] [--json] {remote} {machine}   # why its status; --file: a saved screen
fw hub import FILE.code-workspace... [--name N] [--update] [--json]
```

Agent output:

```bash
fw hub commands SESSION [--last] [--all] [--json] [--copy] [--limit N] {remote} {machine}
fw hub search QUERY [--regex] [--case] [--session S] [--workspace W] [--kind K]... [--source scrollback|transcript|commands]... [--limit N] [--per-session N] [--offset N] [--json] {remote} {machine}   # 1: no hits
fw hub record start SESSION [--json] {remote}
fw hub record stop SESSION [--json] {remote}
fw hub record ls [--json] {remote}
fw hub record get NAME [--output|-o FILE|-] [--force] {remote}
fw hub record rm NAME {remote}
```

Git and agents together:

```bash
fw hub review SESSION [--base REF] [--diff PATH] [--scope committed|staged|unstaged|untracked] [--json] {remote} {machine}
fw hub merge SESSION [--yes|-y] [--rebase] [--remove-worktree] [--abort-rebase] [--json] {remote} {machine}   # ff the base to a worktree branch
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
fw hub fanout rm FANOUT {remote} {machine}
```

Settings and hooks:

```bash
fw hub config check [--file PATH]                   # exit 1 invalid
fw hub config show [--json]
fw hub plugin ls [--json]
fw hub plugin enable NAME
fw hub plugin disable NAME
fw hub plugin logs NAME [--lines|-n N] [--json]
fw hub report --source claude|codex --event E [--detail D] [--stdin] [PAYLOAD]
```

Prompt queues; `ITEM` = a position (1 = next) or an id prefix:

```bash
fw hub queue add SESSION TEXT|- [--when done|idle|blocked-cleared] [--json] {remote} {machine}
fw hub queue ls [SESSION] [--json] {remote} {machine}
fw hub queue rm SESSION ITEM [--json] {remote} {machine}
fw hub queue clear SESSION [--all] [--json] {remote} {machine}
fw hub queue move SESSION ITEM POS [--json] {remote} {machine}
fw hub queue pause SESSION [--json] {remote} {machine}
fw hub queue resume SESSION [--json] {remote} {machine}
```

### Verb notes

- **`new`**: `--create` makes `WS` (with `--folder`) if none matches. `--folder` (alias `--repo`): a path, label, or dir name. Shell `ARGV` replaces `$SHELL`.
- **`send`**: bracketed only when the program enabled it; a final newline is Enter, 50 ms later. `--raw` sends bytes as is (`$'\x03'` is Ctrl-C).
- **`import`**: VS Code `*.code-workspace` (JSONC); each `folders` entry a folder, `name` its label; missing / non-local entries skipped and listed. `--update` replaces a workspace's folders.
- **`open`**: a one-time code via a 0600 redirect page in the state dir (never on argv). Opener: `$BROWSER`, else `wslview`, `cmd.exe /c start`, `xdg-open` / `open`.
- **`url`**: a fresh code under every URL reaching the hub (public, tailnet https, tailnet, lan, this machine); `--qr` draws the first remote one; never the token.
- **`report`**: the hook entry point; needs the session's env, prints nothing, fails with 1 (never 2, which would stop a Claude `Stop`).
- The rest: [hub-workflows](workflows.md) (`wait`, `kill`, `review`, `merge`, `handoff`, `fanout`), [hub-internals](internals.md) (`attach`, `explain`, `upgrade`, `doctor`).

## Serve flags

Taken by `serve`, `start`, `install-service`. All but `--notify-url` are also `hub.toml` keys; a flag wins over the file.

| Flag | Default | |
|---|---|---|
| `--bind` | `127.0.0.1` | non-loopback warns |
| `--port` | `7420` | `0` picks a free port |
| `--public-url` | none | origin browsers reach the hub at via a proxy (no path); repeatable; `url` lists it first |
| `--allow-origin` | none | extra accepted origin; repeatable |
| `--trusted-proxy` | none | a proxy elsewhere whose `X-Forwarded-*` are believed (loopback always); repeatable |
| `--tls` | `off` | `auto` (local name-constrained CA), `off`, or `CERT,KEY` (your PEM files) |
| `--tls-name` | none | extra private name, IP, or network (at most /8) for the local CA; repeatable; added later: `fw hub tls rotate` |
| `--tls-allow-public-name` | none | opt in to a public `--tls-name` you own; repeatable; warned |
| `--scrollback-lines` | `5000` | per session, max 1M cells (24 B each: ~14 MiB at 5000 x 120) |
| `--no-restore` | off | drop pending restores for good |
| `--no-agent-resume` | off | restore agents as fresh shells |
| `--no-flywheel` | off | no Flywheel panel, Promote, or task fan-out |
| `--import-root` | `~/code` | where UI import looks (3 levels) |
| `--notify-url` | none | ntfy push URL; overrides `notify.toml` `url` (shows in `ps`) |
| `--worktree-branch-prefix` | none | worktree branches are `<prefix><task>` |
| `--record-part-cap` | `64MiB` | a part past it rotates (`64KiB`-`4GiB`) |
| `--record-dir-cap` | `1GiB` | recordings cap, oldest finished go first (part cap..`1TiB`) |
| `--share-default-expiry` | `1h` | a link naming none |
| `--share-max-expiry` | `30d` | longest accepted (`60s`-`30d`) |

## hub.toml

`<state-dir>/hub.toml`; **flag > file > default** (a list flag replaces the list). Unknown keys and bad values are errors; `serve` refuses an invalid file (exit 2). Edits are re-read within 1 s: live keys apply, the rest wait for a restart; an invalid edit keeps the last good settings (reported in log, toast, `config check`).

```toml
[server]                          # restart to apply
bind = "127.0.0.1"
port = 7420
public_urls = ["https://box.tailnet.ts.net"]
allow_origins = []
trusted_proxies = []
tls = "off"                       # auto: local name-constrained CA; or "/path/cert.pem,/path/key.pem"
tls_names = []                    # extra names/IPs/networks for it, e.g. ["192.168.0.0/16"]
tls_public_names = []             # public tls_names entries you own and opt in to (a warning)

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
| `server.tls` | `--tls` | `"off"` (CERT,KEY absolute) | restart |
| `server.tls_names` | `--tls-name` | `[]` (private only) | restart |
| `server.tls_public_names` | `--tls-allow-public-name` | `[]` | restart |
| `sessions.scrollback_lines` | `--scrollback-lines` | `5000` (new sessions) | live |
| `sessions.restore` | `--no-restore` | `true` | restart |
| `sessions.agent_resume` | `--no-agent-resume` | `true` | restart |
| `worktrees.branch_prefix` | `--worktree-branch-prefix` | `""` | live |
| `import.root` | `--import-root` | `"~/code"` | live |
| `flywheel.enabled` | `--no-flywheel` | `true` | restart |
| `agents.claude.model` | - | none (fills launches naming none) | live |
| `agents.claude.permission_mode` | - | none | live |
| `agents.codex.model` | - | none | live |
| `agents.codex.approval` | - | none | live |
| `agents.codex.sandbox` | - | none | live |
| `plugins.enabled` | - | `true` | live |
| `plugins.max_concurrent` | - | `4` (1-16) | restart |
| `recordings.part_cap` | `--record-part-cap` | `"64MiB"` (running ones too) | live |
| `recordings.dir_cap` | `--record-dir-cap` | `"1GiB"` | live |
| `shares.default_expiry` | `--share-default-expiry` | `"1h"` (new links) | live |
| `shares.max_expiry` | `--share-max-expiry` | `"30d"` | live |
| `review.reviewer` | - | `"other"` | live |
| `queue.typing_quiet` | - | `"8s"` (1s-10m) | live |

Elsewhere: push (`notify.toml`: secrets), detection overrides (`agent-detection/`), plugins (`plugins/`), UI settings (`settings.json`).

## notify.toml

`<state-dir>/notify.toml`, must be `chmod 600`. Behaviour: [hub-internals](internals.md#push-notifications).

```toml
url = "https://ntfy.example.com/hub"
token = "tk_..."               # optional Bearer (or user:pass@ in the URL: Basic)
events = ["blocked", "done"]   # default both
debounce_secs = 30             # one push per session and status per window
settle_secs = 3                # a status holds this long before it pushes (max 60)
detail = false                 # leave out what a blocked agent asks (default true)
```

## plugin.toml

**A plugin runs as you, unsandboxed; the hub reviews nothing.** Install = copy a directory to `<state-dir>/plugins/<name>/` (nothing downloads one); **disabled** until `fw hub plugin enable NAME`; re-scanned every 2 s. Sample: `crates/flywheel-hub/plugins/hello`. Protocol: [hub-api](api.md#plugins).

```toml
description = "Ping me when an agent is stuck"
command = ["./hook.sh"]            # argv, no shell; ./x in the plugin dir, bare names on PATH
timeout_secs = 10                  # 1-300
[[events]]
on = "status.changed"              # any event in api.md#plugin-events
statuses = ["blocked", "done"]     # optional (status.changed)
kinds = ["claude", "codex"]        # optional (never matches fanout.finished)
[[actions]]                        # palette "Plugin: <title>", Settings > Plugins
id = "summarize"
title = "Summarize this session"
context = "session"                # needs an attached session (default "hub")
command = ["./summarize.py"]       # optional: else the top-level one
```

## UI settings

`Alt ,` or the sidebar gear. Stored by the hub (`settings.json`), so they follow you; any can be kept for **this device** only.

| Section | |
|---|---|
| Appearance | theme, terminal font, text size (13, phones 11), density |
| Terminal | client scrollback (the hub's: `sessions.scrollback_lines`), cursor style / blink, ligatures outside terminals (off) |
| Keyboard | pane prefix, key bar, companion key mode, tour; every shortcut |
| Notifications | system notifications (device), command toasts |
| Agents | the new-session form's defaults per kind |
| Plugins | per plugin: enable, events, actions, last run / error, logs |
| Server | read-only: effective hub.toml, sources, errors, pending restart |

**Key bindings**: up to 4 per action, `mod+...+key` (`mod` = Ctrl, Cmd on macOS). Refused (422): keys the browser or OS keep (`ctrl+w`, `ctrl+t`, `alt+tab`, `meta+`), keys terminals need (no modifier, `ctrl+c`, `ctrl+d`, ...), duplicates in a scope. Warned: keys shells lose (`alt+b/f`, `ctrl+<letter>`) or desktops take (`ctrl+alt+t`).

## Keyboard shortcuts

Defaults, all rebindable. `mod` = Ctrl (Cmd on macOS); `alt` = Alt (Ctrl Option on macOS); `prefix.*` keys follow `prefix.arm` (twice sends `^B`).

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

Also: `F2` / double-click renames and `Delete` closes a focused sidebar row.

## Environment

| Variable | |
|---|---|
| `FW_HUB_TOKEN` | token for `{remote}` verbs |
| `FW_HUB_MACHINE_SECRET` | `machine add` secret |
| `FLYWHEEL_PG_DSN`, `DATABASE_URL` | the Flywheel panel's Postgres DSN (the hub's environment) |
| `BROWSER` | `fw hub open`'s opener |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | transcripts / rollouts (default `~/.claude`, `~/.codex`) |
| set in sessions | `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`; agents also `FLYWHEEL_HUB_REPORT_SOCKET`, `FLYWHEEL_HUB_REPORT_SECRET` |

## State dir

All files 0600, the dir 0700.

| Path | |
|---|---|
| `token` | access token |
| `logins.json` | login-cookie digests |
| `state.json` | workspaces, sessions, layouts, templates, fan-outs, queues (schema 5) |
| `settings.json` | UI settings |
| `hub.toml`, `notify.toml` | above |
| `server.json`, `serve.lock`, `report.sock`, `handoff.sock` | running hub's address, lock, hook and handoff sockets |
| `hub.log`, `hub.log.1..3` | detached hub log |
| `machines.json`, `peers.json`, `shares.json` | machines (plaintext credentials), peer and share digests |
| `commands/<id>.json`, `usage-archive.json` | suggested blocks + usage per session; removed sessions' usage |
| `recordings/`, `plugins/`, `plugins.json`, `plugin-logs/`, `agent-detection/`, `tls/` | recordings, plugins (+ enablement, logs), detection overrides, local CA |
| `open-<id>.html` | `open` redirect pages |

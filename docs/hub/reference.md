# Hub reference

Every `fw hub` verb and flag and the hub's files (`docs_drift.rs` fails the build on drift). Front door: [hub](../hub.md).

## Conventions

- `SESSION` / `WS`: id, unique id prefix, or name (an exact name beats a prefix; ambiguous or none: error; `activity` also takes names its journal or a machine knows).
- A closed output pipe (`| head -1`) ends a verb quietly, exit 0.
- Every verb but `report` takes `--state-dir DIR` (default `$XDG_STATE_HOME/flywheel/hub`, else `~/.local/state/flywheel/hub`).
- `{remote}` = `[--url URL] [--token T] [--fingerprint SHA256]`: a hub elsewhere; prefer `FW_HUB_TOKEN` (argv is world-readable).
- `{machine}` = `[--machine NAME]`: a machine registered on that hub, via its proxy; `{where}` = `{remote} {machine}`.
- `{serve}` = the [serve flags](#serve-flags).

## Verbs

Server and lifecycle:

```bash
fw hub start {serve} [--timeout SECS] [--handoff]  # detached; prints login URLs
fw hub stop [--timeout SECS] [--force]
fw hub upgrade [--binary PATH] [--allow-restart] [--timeout SECS]  # exit 4 refused, 3 no hub
fw hub status [--json]  # exit 3 not running
fw hub logs [--follow|-f] [--lines|-n N]  # -n 0: all
fw hub serve {serve} [--handoff]  # foreground
fw hub install-service {serve} [--name N] [--force] [--print]  # systemd user unit
fw hub doctor [--port N] [--json]  # exit 1 on a FAIL
```

Sign-in, access, other hubs:

```bash
fw hub open [--session S] {where}
fw hub url [--qr] {remote}
fw hub token [--rotate]
fw hub tls ca [--out FILE] [--der]
fw hub tls rotate
fw hub peer issue --name X [--json]
fw hub peer ls [--json]
fw hub peer revoke NAME|ID
fw hub machine add NAME URL [--mint] [--peer-name N] [--fingerprint SHA256] [--replace] [--json]  # secret: env or stdin
fw hub machine ls [--no-probe] [--json]
fw hub machine rm NAME [--json]
fw hub machine test [NAME] [--json]
fw hub share SESSION [--expires D] [--name LABEL] [--qr] [--json] {remote}
fw hub share ls [--json] {remote}
fw hub share revoke [ID|LABEL] [--all] [--session S] {remote}
```

Sessions:

```bash
fw hub ls [--json] {where}
fw hub new WS [--kind shell|claude|codex] [--folder PATH|LABEL] [--name N] [--create] [--worktree TASK] [--base REF] [--json] {where} [-- ARGV...]
    [--prompt TEXT|-] [--model M] [--permission-mode P] [--approval A] [--sandbox S] [--template NAME]
fw hub template ls [WS] [--json] {where}
fw hub template save WS NAME [--kind claude|codex] [--prompt TEXT|-] [--model M] [--permission-mode P] [--approval A] [--sandbox S] [--worktree] [--no-worktree] [--replace] [--edit] {where}
fw hub template rm WS NAME {where}
fw hub send SESSION TEXT|- [--enter] [--raw] {where}
fw hub read SESSION [--source visible|recent] [--lines N] [--json] {where}
fw hub wait SESSION --until done|blocked|idle|working|exit [--timeout SECS] [--changed] {where}
fw hub attach [SESSION] [--prefix C-b] [--no-status] {where}
fw hub kill SESSION [--rm] [--remove-worktree] [--force] {where}  # --rm forgets
fw hub relaunch SESSION {where}
fw hub explain [SESSION] [--file SCREEN.txt|-] [--kind claude|codex|shell] [--json] {where}
fw hub import FILE.code-workspace... [--name N] [--update] [--json]
```

Agent output:

```bash
fw hub commands SESSION [--last] [--all] [--json] [--copy] [--limit N] {where}
fw hub search QUERY [--regex] [--case] [--session S] [--workspace W] [--kind K]... [--source scrollback|transcript|commands]... [--limit N] [--per-session N] [--offset N] [--json] {where}  # 1: no hits
fw hub activity [--since AGE|TIME|last-seen] [--until T] [--workspace W] [--session S] [--kind K]... [--limit N] [--json] {where}
fw hub record start SESSION [--json] {remote}
fw hub record stop SESSION [--json] {remote}
fw hub record ls [--json] {remote}
fw hub record get NAME [--output|-o FILE|-] [--force] {remote}
fw hub record rm NAME {remote}
```

Git and agents together:

```bash
fw hub review SESSION [--base REF] [--diff PATH] [--scope committed|staged|unstaged|untracked] [--json] {where}
fw hub merge SESSION [--yes|-y] [--rebase] [--remove-worktree] [--abort-rebase] [--json] {where}
fw hub handoff FROM TO (--selection-file F|- | --diff [--path P] [--base REF] | --last-command | --command SEQ | --message N) [--to-machine M] [--max-bytes N] [--edit] [--dry-run] [--json] {where}
fw hub ask-review SESSION [--kind claude|codex] [--base REF] [--model M] [--dry-run] [--findings] [--send-findings] [--json] {where}
fw hub fanout new WS (--prompt TEXT|- --grader [name=]CMD... | --from-task ID) --agent KIND[:MODEL]... [--folder F] [--base BRANCH] [--name N] [--timeout SECS] [--protect PATH]... [--permission-mode M] [--approval A] [--sandbox S] [--no-commit-instruction] [--json] {where}
fw hub fanout ls [--json] {where}
fw hub fanout show FANOUT [--verbose|-v] [--json] {where}
fw hub fanout wait FANOUT [--timeout SECS] [--json] {where}
fw hub fanout grade FANOUT CAND {where}
fw hub fanout fix FANOUT CAND [--yes|-y] [--json] {where}
fw hub fanout pick FANOUT CAND [--yes|-y] [--rebase] [--json] {where}
fw hub fanout promote FANOUT [--task-id ID] [--phase NAME] [--goal TEXT] [--reference CAND|none] [--yes|-y] [--json] {where}
fw hub fanout cleanup FANOUT [CAND...] [--stop-only] [--force] [--yes|-y] [--json] {where}
fw hub fanout rm FANOUT {where}
```

Settings and hooks:

```bash
fw hub config check [--file PATH]
fw hub config show [--json]
fw hub plugin ls [--json]
fw hub plugin enable NAME
fw hub plugin disable NAME
fw hub plugin logs NAME [--lines|-n N] [--json]
fw hub report --source claude|codex --event E [--detail D] [--stdin] [PAYLOAD]
fw hub perms show FOLDER|SESSION [--json] {remote}
fw hub perms allow FOLDER|SESSION RULE... [--file local|project|user] [--agent claude|codex] [--gitignore] [--no-answer] [--yes] [--json] {remote}
fw hub perms suggest FOLDER|SESSION [--days N] [--json] {remote}
fw hub perms codex FOLDER|SESSION trust_level|approval_policy|sandbox_mode VALUE [--yes] [--json] {remote}
```

Prompt queues; `ITEM` = a position (1 = next) or an id prefix:

```bash
fw hub queue add SESSION TEXT|- [--when done|idle|blocked-cleared] [--json] {where}
fw hub queue ls [SESSION] [--json] {where}
fw hub queue rm SESSION ITEM [--json] {where}
fw hub queue clear SESSION [--all] [--json] {where}
fw hub queue move SESSION ITEM POS [--json] {where}
fw hub queue pause SESSION [--json] {where}
fw hub queue resume SESSION [--json] {where}
```

### Verb notes

| Verb | |
|---|---|
| `new` | `--create` makes `WS` (`--folder`: path, label, or dir name; alias `--repo`); shell `ARGV` replaces `$SHELL` |
| `send` | bracketed only when the program enabled it; a final newline is Enter, 50 ms later; `--raw`: bytes as is (`$'\x03'` is Ctrl-C) |
| `import` | VS Code `*.code-workspace` (JSONC) `folders` (non-local skipped, listed); `--update` replaces them |
| `open` | a one-time code via a 0600 redirect page (never argv); opener `$BROWSER`, else `wslview`, `cmd.exe /c start`, `xdg-open` / `open` |
| `url` | a fresh code under every URL reaching the hub; `--qr` draws the first remote one; never the token |
| `report` | the hook entry point: needs the session's env, prints nothing, fails with 1 (2 would stop a Claude `Stop`) |

The rest: [hub-workflows](workflows.md), [hub-internals](internals.md).

## Serve flags

Taken by `serve`, `start`, `install-service`. All but `--notify-url` are `hub.toml` keys (defaults [below](#hubtoml)); a flag wins.

| Flag | |
|---|---|
| `--bind` | non-loopback warns |
| `--port` | `0` picks a free port |
| `--public-url` | origin browsers reach the hub at via a proxy; repeatable |
| `--allow-origin` | extra accepted origin; repeatable |
| `--trusted-proxy` | a proxy elsewhere whose `X-Forwarded-*` are believed (loopback always); repeatable |
| `--tls` | `auto` (local name-constrained CA), `off`, or `CERT,KEY` (your PEM files) |
| `--tls-name` | extra private name, IP, or network (at most /8) for the local CA; repeatable |
| `--tls-allow-public-name` | opt in to a public `--tls-name` you own; repeatable; warned |
| `--scrollback-lines` | per session, max 1M cells (~14 MiB at 5000 x 120) |
| `--no-restore` | drop pending restores for good |
| `--no-agent-resume` | restore agents as fresh shells |
| `--no-flywheel` | no Flywheel panel, Promote, or task fan-out |
| `--import-root` | where UI import looks (3 levels) |
| `--notify-url` | ntfy push URL; overrides `notify.toml` `url` (shows in `ps`) |
| `--worktree-branch-prefix` | worktree branches are `<prefix><task>` |
| `--record-part-cap` | a part past it rotates (`64KiB`-`4GiB`) |
| `--record-dir-cap` | recordings cap, oldest finished go first (part cap..`1TiB`) |
| `--share-default-expiry` | a link naming none |
| `--share-max-expiry` | longest accepted (`60s`-`30d`) |

## hub.toml

`<state-dir>/hub.toml`; **flag > file > default** (a list flag replaces the list). Unknown keys and bad values are errors (`serve` exits 2). Edits are re-read within 1 s: live keys apply, the rest at restart; an invalid edit keeps the last good.

```toml
[server]
bind = "127.0.0.1"
port = 7420
public_urls = ["https://box.tailnet.ts.net"]
allow_origins = []
trusted_proxies = []
tls = "off"                # "auto", or "/path/cert.pem,/path/key.pem"
tls_names = []             # e.g. ["192.168.0.0/16"]
tls_public_names = []

[sessions]
scrollback_lines = 5000
restore = true
agent_resume = true

[worktrees]
branch_prefix = "agent/"

[import]
root = "~/code"

[flywheel]
enabled = true

[agents.claude]            # a launch naming none gets these
model = "opus"
permission_mode = "acceptEdits"
[agents.codex]
model = "gpt-5-codex"
approval = "on-request"
sandbox = "workspace-write"

[plugins]
enabled = true
max_concurrent = 4

[recordings]
part_cap = "64MiB"
dir_cap = "1GiB"

[shares]
default_expiry = "1h"
max_expiry = "30d"

[review]
reviewer = "other"         # other (claude <-> codex) | same | claude | codex

[queue]
typing_quiet = "8s"        # no typing this long before a queue delivers
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

`<state-dir>/notify.toml`, `chmod 600`. Behaviour: [hub-internals](internals.md#push-notifications).

```toml
url = "https://ntfy.example.com/hub"
token = "tk_..."  # optional Bearer (or user:pass@ in the URL: Basic)
events = ["blocked", "done"]  # default both
debounce_secs = 30  # one push per session and status per window
settle_secs = 3  # a status holds this long before it pushes (max 60)
detail = false  # leave out what a blocked agent asks (default true)
```

## plugin.toml

**A plugin runs as you, unsandboxed.** Copy a directory to `<state-dir>/plugins/<name>/`; **disabled** until `fw hub plugin enable NAME`; re-scanned every 2 s. Sample: `crates/flywheel-hub/plugins/hello`. Protocol: [hub-api](api.md#plugins).

```toml
description = "Ping me when an agent is stuck"
command = ["./hook.sh"]  # argv, no shell; ./x in the plugin dir, bare names on PATH
timeout_secs = 10  # 1-300
[[events]]
on = "status.changed"  # any event in api.md#plugin-events
statuses = ["blocked", "done"]  # optional (status.changed)
kinds = ["claude", "codex"]  # optional (never matches fanout.finished)
[[actions]]  # palette "Plugin: <title>", Settings > Plugins
id = "summarize"
title = "Summarize this session"
context = "session"  # needs an attached session (default "hub")
command = ["./summarize.py"]  # optional: else the top-level one
```

## UI settings

`Alt ,` or the sidebar gear; stored by the hub (`settings.json`), any can be kept for **this device** only.

- **Appearance**: theme, terminal font, text size (13, phones 11), density. **Terminal**: client scrollback, cursor style / blink, ligatures (off), lite mode (auto; per device).
- **Keyboard**: pane prefix, key bar, companion key mode, tour, every shortcut. **Notifications**: system (device), command toasts. **Agents**: new-session defaults per kind.
- **Plugins**: enable, events, actions, last run / error, logs. **Server** (read-only): effective hub.toml, sources, errors, pending restart.

**Key bindings**: up to 4 per action, `mod+...+key`. Refused (422): keys the browser/OS keep (`ctrl+w`, `ctrl+t`, `alt+tab`, `meta+`), keys terminals need (no modifier, `ctrl+c`, `ctrl+d`, ...), duplicates in a scope. Warned: `alt+b/f`, `ctrl+<letter>`.

## Keyboard shortcuts

Defaults, all rebindable (Settings > Keyboard names each). `mod` = Ctrl (Cmd on macOS); `alt` = Alt (Ctrl Option on macOS); `prefix.*` keys follow `prefix.arm` when the pane prefix is on (twice sends `^B`); `palette.open` pressed inside the palette goes to the terminal.

| Action | Default |
|---|---|
| `palette.open` | `mod+k` |
| `search.open` | `mod+shift+f` |
| `settings.open` | `alt+,` |
| `keys.show` | - |
| `session.new` | `alt+n` |
| `workspace.new` | `alt+shift+n` |
| `session.jump` | `alt+1..9` |
| `session.prev` | `alt+up` |
| `session.next` | `alt+down` |
| `session.rename` | `alt+r` |
| `session.close` | `alt+w` |
| `share.open` | - |
| `record.toggle` | - |
| `commands.toggle` | `alt+c` |
| `commands.copy_last` | `alt+shift+c` |
| `companion.flip` | `` alt+` `` |
| `flywheel.toggle` | `alt+f` |
| `review.toggle` | `alt+g` |
| `handoff.send` | - |
| `review.ask` | - |
| `queue.toggle` | `alt+q` |
| `fanout.open` | - |
| `fanout.new` | - |
| `pane.split_right` | `alt+\` |
| `pane.split_down` | `alt+-` |
| `pane.focus_left` | `alt+shift+left` |
| `pane.focus_down` | `alt+shift+down` |
| `pane.focus_up` | `alt+shift+up` |
| `pane.focus_right` | `alt+shift+right` |
| `pane.resize_left` | `alt+shift+h` |
| `pane.resize_down` | `alt+shift+j` |
| `pane.resize_up` | `alt+shift+k` |
| `pane.resize_right` | `alt+shift+l` |
| `pane.zoom` | `alt+z` |
| `pane.close` | `alt+x` |
| `tab.new` | `alt+t` |
| `tab.prev` | `alt+pageup` |
| `tab.next` | `alt+pagedown` |
| `prefix.arm` | `ctrl+b` |
| `prefix.split_right` | `%` |
| `prefix.split_down` | `"` |
| `prefix.focus_left` | `left` |
| `prefix.focus_down` | `down` |
| `prefix.focus_up` | `up` |
| `prefix.focus_right` | `right` |
| `prefix.resize_left` | `ctrl+left` |
| `prefix.resize_down` | `ctrl+down` |
| `prefix.resize_up` | `ctrl+up` |
| `prefix.resize_right` | `ctrl+right` |
| `prefix.zoom` | `z` |
| `prefix.close_pane` | `x` |
| `prefix.new_tab` | `c` |
| `prefix.next_tab` | `n` |
| `prefix.prev_tab` | `p` |
| `prefix.tab` | `1..9` |
| `prefix.rename_tab` | `,` |
| `prefix.close_tab` | `&` |
| `prefix.next_pane` | `o` |
| `prefix.help` | `?` |

Also: `F2` / double-click renames and `Delete` closes a focused sidebar row.

## Environment

| Variable | |
|---|---|
| `FW_HUB_TOKEN` | token for `{remote}` verbs |
| `FW_HUB_MACHINE_SECRET` | `machine add` secret |
| `FLYWHEEL_PG_DSN`, `DATABASE_URL` | the Flywheel panel's Postgres DSN |
| `BROWSER` | `fw hub open`'s opener |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | transcripts / rollouts (default `~/.claude`, `~/.codex`) |
| set in sessions | `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`; agents `FLYWHEEL_HUB_REPORT_SOCKET`, `FLYWHEEL_HUB_REPORT_SECRET` |

## State dir

All files 0600, the dir 0700.

| Path | |
|---|---|
| `token` | access token |
| `logins.json` | login-cookie digests |
| `state.json` | workspaces, sessions, layouts, templates, fan-outs, queues (schema 5) |
| `settings.json` | UI settings |
| `hub.toml`, `notify.toml` | above |
| `server.json`, `serve.lock`, `report.sock`, `handoff.sock` | running hub's address, lock, sockets |
| `hub.log`, `hub.log.1..3` | detached hub log |
| `machines.json`, `peers.json`, `shares.json` | machines (plaintext credentials), peer and share digests |
| `commands/<id>.json`, `usage-archive.json` | suggested blocks + usage; removed sessions' usage |
| `recordings/`, `plugins/`, `plugins.json`, `plugin-logs/`, `agent-detection/`, `tls/` | as named; `tls/`: local CA |
| `activity.jsonl`, `activity.<n>.jsonl`, `activity-clients.json` | activity journal; each client's last-active time |
| `open-<id>.html` | `open` redirect pages |

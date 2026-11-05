# Hub reference

Every `fw hub` verb and flag and the hub's files (`docs_drift.rs` checks). Front door: [hub](../hub.md).

## Conventions

- `SESSION` / `WS`: id, unique id prefix, or name (an exact name beats a prefix; `activity` also takes names its journal or a machine knows).
- A closed output pipe (`| head -1`) ends a verb quietly, exit 0.
- Exit codes: 0 ok, 1 failed, 2 bad usage/config, 3 no hub running (`no hub is running for state dir D`), 4 handoff refused as incompatible, 5 hub busy (retry). `doctor`, `tls`, `config` work without a hub (exit 0) and say so.
- Every verb but `report` takes `--state-dir DIR` (default `$XDG_STATE_HOME/flywheel/hub`, else `~/.local/state/flywheel/hub`).
- `{remote}` = `[--url URL] [--token T] [--fingerprint SHA256]`: a hub elsewhere; prefer `FW_HUB_TOKEN` (argv is world-readable).
- `{machine}` = `[--machine NAME]`: a machine registered on that hub, via its proxy; `{where}` = `{remote} {machine}`.
- `{serve}` = the [serve flags](#serve-flags).

## Verbs

Server and lifecycle:

```bash
fw hub start {serve} [--timeout SECS] [--handoff]  # detached; prints login URLs; --handoff exits as upgrade
fw hub stop [--timeout SECS] [--force]  # exit 3 none running
fw hub upgrade [--binary PATH] [--allow-restart] [--timeout SECS]  # exit 1 failed (hub carries on), 3, 4, 5
fw hub status [--json]  # exit 3 not running, 1 not answering
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
fw hub digest [--since AGE|TIME] [--send] [--json] {where}  # a preview unless --send
fw hub record start SESSION [--json] {remote}
fw hub record stop SESSION [--json] {remote}
fw hub record ls [--json] {remote}
fw hub record get NAME [--output|-o FILE|-] [--force] {remote}
fw hub record rm NAME {remote}
```

Git and agents together:

```bash
fw hub review SESSION [--base REF] [--diff PATH] [--scope committed|staged|unstaged|untracked] [--json] {where}
fw hub merge SESSION [--yes|-y] [--rebase] [--remove-worktree] [--abort-rebase] [--include-suspects] [--json] {where}
fw hub pr SESSION [--refresh] [--json] {where}
fw hub pr log SESSION CHECK [--json] {where}
fw hub pr fix-ci SESSION [--check NAME] [--yes|-y] [--json] {where}
fw hub pr rerun SESSION [--yes|-y] [--json] {where}
fw hub pr ready SESSION [--yes|-y] [--json] {where}
fw hub pr merge SESSION [--yes|-y] [--force NUMBER] [--json] {where}
fw hub handoff FROM TO (--selection-file F|- | --diff [--path P] [--base REF] | --last-command | --command SEQ | --message N) [--to-machine M] [--max-bytes N] [--edit] [--dry-run] [--json] {where}
fw hub ask-review SESSION [--kind claude|codex] [--base REF] [--model M] [--dry-run] [--findings] [--send-findings] [--json] {where}
fw hub fanout new WS (--prompt TEXT|- --grader [name=]CMD... | --from-task ID) --agent KIND[:MODEL]... [--folder F] [--base BRANCH] [--name N] [--timeout SECS] [--protect PATH]... [--permission-mode M] [--approval A] [--sandbox S] [--no-commit-instruction] [--json] {where}
fw hub fanout ls [--json] {where}
fw hub fanout show FANOUT [--verbose|-v] [--json] {where}
fw hub fanout wait FANOUT [--timeout SECS] [--json] {where}
fw hub fanout grade FANOUT CAND {where}
fw hub fanout fix FANOUT CAND [--yes|-y] [--json] {where}
fw hub fanout pick FANOUT CAND [--yes|-y] [--rebase] [--include-suspects] [--json] {where}
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
fw hub broadcast TARGET... TEXT|- [--header] [--json] {where}  # exit 1: any refused
```

A folder's autopilot (`start`, `approve`, `deny` ask; `--yes` skips):

```bash
fw hub autopilot FOLDER status [--json] {where}
fw hub autopilot FOLDER start [--workspace ID] [--yes|-y] [--json] {where}
fw hub autopilot FOLDER stop [--json] {where}  # Ctrl-C
fw hub autopilot FOLDER pause [--json] {where}
fw hub autopilot FOLDER resume [--json] {where}
fw hub autopilot FOLDER approve RUN [--gate N] [--attempt N] [--yes] [--json] {where}
fw hub autopilot FOLDER deny RUN [--feedback TEXT] [--gate N] [--attempt N] [--yes] [--json] {where}
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

Taken by `serve`, `start`, `install-service`; each sets the [hub.toml](#hubtoml) key of its row there (a flag wins), but `--notify-url`: the ntfy push URL over `notify.toml` `url` (shows in `ps`: prefer the file).

| Flag | |
|---|---|
| `--bind` | non-loopback warns |
| `--port` | `0` picks a free port |
| `--public-url` | origin browsers reach the hub at via a proxy; repeatable |
| `--allow-origin` | extra accepted origin; repeatable |
| `--trusted-proxy` | a proxy elsewhere whose `X-Forwarded-*` are believed (loopback always); repeatable |
| `--tls` | `auto` (local name-constrained CA), `off`, or `CERT,KEY` (PEM files) |
| `--tls-name` | extra private name, IP, or network (at most /8) for the local CA; repeatable |
| `--tls-allow-public-name` | a public `--tls-name` you own; repeatable; warned |
| `--scrollback-lines` | per session, max 1M cells (~14 MiB at 5000 x 120) |
| `--no-restore` | drop pending restores for good |
| `--no-agent-resume` | restore agents as fresh shells |
| `--no-flywheel` | no Flywheel panel, Promote, or task fan-out |
| `--import-root` | where UI import looks (3 levels) |
| `--notify-url` | above |
| `--worktree-branch-prefix` | branches are `<prefix><task>` |
| `--record-part-cap` | a part past it rotates (`64KiB`-`4GiB`) |
| `--record-dir-cap` | oldest finished go first (part cap..`1TiB`) |
| `--share-default-expiry` | a link naming none |
| `--share-max-expiry` | longest accepted (`60s`-`30d`) |
| `--login-days` | a browser login lasts this long after its last use |

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
login_days = 30            # a login lasts this long after its last use

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

[digest]                   # scheduled summaries; off until times are set
times = ["08:00", "18:00"] # local times in timezone
timezone = "Europe/Berlin" # IANA name; default the system's
channels = ["push", "browser", "plugin"]
quiet_if_empty = true      # skip a scheduled one with nothing in it
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
| `server.login_days` | `--login-days` | `30` (1-365; new logins, and each at its next use) | live |
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
| `digest.times` | - | `[]` (off; at most 8) | live |
| `digest.timezone` | - | the system's, else UTC | live |
| `digest.channels` | - | all three | live |
| `digest.quiet_if_empty` | - | `true` | live |

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
kinds = ["claude", "codex"]  # optional (broadcast.sent: any target; never fanout.finished)
[[actions]]  # palette "Plugin: <title>", Settings > Plugins
id = "summarize"
title = "Summarize this session"
context = "session"  # needs an attached session (default "hub")
command = ["./summarize.py"]  # optional: else the top-level one
```

## UI settings

`Alt ,` or the sidebar gear; stored by the hub (`settings.json`), any can be kept for **this device** only.

Sections: Appearance, Terminal (incl. lite mode), Keyboard (pane prefix, key bar, shortcuts), Notifications, Agents (new-session defaults), Plugins (enable, runs, logs), Digest (writes hub.toml `[digest]` only), Server (effective hub.toml, read-only).

**Key bindings**: up to 4 per action, `mod+...+key`. Refused (422): keys the browser/OS keep (`ctrl+w`, `ctrl+t`, `alt+tab`, `meta+`), keys terminals need (no modifier, `ctrl+c`, `ctrl+d`, ...), duplicates in a scope. Warned: `alt+b/f`, `ctrl+<letter>`.

## Keyboard shortcuts

Defaults, all rebindable (Settings > Keyboard). `mod` = Ctrl (Cmd on macOS); `alt` = Alt (Ctrl Option on macOS); `prefix.*` keys follow `prefix.arm` when the pane prefix is on (twice sends `^B`).

| Action | Default | Does |
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
| `FLYWHEEL_PG_DSN`, `DATABASE_URL` | the Flywheel panel's Postgres DSN |
| `BROWSER` | `fw hub open`'s opener |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | transcripts / rollouts (default `~/.claude`, `~/.codex`) |
| set in sessions | `FLYWHEEL_HUB_SESSION`, `FLYWHEEL_HUB_WORKSPACE`, `TERM=xterm-256color`; agents `FLYWHEEL_HUB_REPORT_SOCKET`, `FLYWHEEL_HUB_REPORT_SECRET` |

## State dir

All files 0600, the dir 0700.

- `token`; `logins.json` (login-cookie digests); `state.json` (workspaces, sessions, layouts, templates, fan-outs, queues; schema 5); `settings.json`; `hub.toml`, `notify.toml`.
- `server.json`, `serve.lock`, `report.sock`, `handoff.sock` (the running hub's address, lock, sockets); `hub.log[.1..3]`.
- `machines.json` (plaintext credentials), `peers.json`, `shares.json` (digests); `commands/<id>.json` (suggested blocks + usage), `usage-archive.json` (removed sessions').
- `recordings/`, `plugins/`, `plugins.json`, `plugin-logs/`, `agent-detection/`, `tls/` (local CA); `activity.jsonl`, `activity.<n>.jsonl`, `activity-clients.json` (clients' last-active times); `digest.json`, `digest.lock` (last slot, recent digests); `open-<id>.html` (`open` redirect pages).

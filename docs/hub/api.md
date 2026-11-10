# Hub API

Routes (`docs_drift.rs`: exactly those mounted), WebSockets, plugin payloads. Front door: [hub](../hub.md).

## Conventions

- JSON under `/api`: `Authorization: Bearer <token>`, a login cookie, or a peer credential ([hub-security](security.md#auth)).
- Errors `{"error"}`: 400 malformed, 404 unknown, 405 method, 409 refused by state, 422 wrong JSON shape. `cols`/`rows` 0: 120x32.
- Tables: bodies, status codes; full responses: `curl -sH "Authorization: Bearer $(cat <state-dir>/token)" localhost:7420/api/sessions | jq`, or a verb's `--json`.

## Workspaces

| Method | Path | |
|---|---|---|
| GET / POST | `/api/workspaces` | list (with `templates`) / create `{"name","folders"}` |
| GET / PATCH / DELETE | `/api/workspaces/{id}` | PATCH `name` / `folders`; 409 a running session's folder / sessions running |
| GET / PUT | `/api/workspaces/{id}/layout` | `{"rev","tabs":[{"id","name","root"}]}`; node `{"type":"pane","id","session"?}` or `{"type":"split","dir":"right\|down","ratio","a","b"}`; tab root may be `{"type":"board","id","folder"}` (workspace folder, one each); PUT -> `rev`+1; 409 stale; 400 invalid (ratio 0.05..0.95, 12 tabs / 12 panes) |
| POST | `/api/workspaces/import` | `{"path","name"?,"update"?}` (hub-side file): 201 / 200 updated / 409 taken |
| GET | `/api/code-workspaces[?root=]` | `.code-workspace` files under `--import-root` |
| GET | `/api/folders/check?path=` | `ok`, `git` / `error` |
| GET | `/api/repos/check` | old name |
| GET | `/api/fs/dirs?path=[&prefix=]` | subdirectories (never files): `path`, `parent`, `entries:[{"name","path","git","hidden","link"?,"error"?}]`, `truncated` (2000) |
| GET | `/api/fs/suggestions` | `suggestions:[{"path","source":"used\|found","workspace"?}]`: used folders, then repos found under `~` (cached 60 s) |
| GET | `/api/templates[?workspace_id=]` | launch templates |
| POST | `/api/workspaces/{id}/templates` | create (a taken `name`: 409 unless `"replace":true`) |
| DELETE | `/api/workspaces/{id}/templates/{name}` | |

`folders` input: paths or `{"path","name"}` (`""` clears the label); listed with `git`, `missing`.

## Sessions

| Method | Path | |
|---|---|---|
| GET / POST | `/api/sessions[?workspace_id=]` | list / create `{"workspace_id","folder"?,"kind"?,"command"?:[argv],"worktree"?:{"task","base"?},"prompt"?,...}` (`new`'s flags) |
| GET / PATCH / DELETE | `/api/sessions/{id}` | PATCH `{"name"}`; DELETE kills, forgets (204; `?remove_worktree=true[&force=true]`) |
| POST | `/api/sessions/{id}/kill` | 202: SIGHUP (3 s: SIGKILL), kept |
| POST | `/api/sessions/{id}/relaunch` | `{"cols"?,"rows"?}`: an ended session, same id; 409 running |
| POST | `/api/sessions/{id}/restart` | old `/relaunch` |
| GET | `/api/sessions/{id}/screen[?source=visible\|recent&lines=N]` | rendered text: `visible` rows, or `recent` lines (1..100000, 100); cursor, modes |
| GET | `/api/sessions/{id}/explain` | as `fw hub explain --json` |
| POST | `/api/sessions/{id}/input` | `{"data","paste"?,"enter"?}`; 409 not running / not reading |
| POST | `/api/sessions/{id}/paste` | `{"text","run"?}`: `/input` with `paste`, trailing newlines dropped, 1 MiB; 409 unsafe multi-line |
| GET | `/api/sessions/{id}/commands[?limit=N&all=true]` | newest first (shell unless `all`; `limit` 1..500, 50) |
| GET | `/api/sessions/{id}/replies[?limit=N]` | newest first (`limit` 1..100, 20): `text`, `blocks`, `match_key` |
| GET | `/api/sessions/{id}/file?path=` | a file the session names: relative to its cwd, worktree, or workspace folders, or absolute / `~/`; must resolve (symlinks too) under those; `{"path","size","text","truncated"}` (first 2 MiB); 403 outside, 404 none, 415 binary |
| POST | `/api/sessions/{id}/files/check` | `{"paths":[...]}` (64): `{"files":[{"path","size"}\|null]}`, in order |
| GET | `/api/sessions/{id}/transcript[?around=N&before=N&after=N]` | messages, paged (30, max 200); 400 a shell |
| POST | `/api/sessions/{id}/companion` | `{"text"?,"run"?}`: companion shell (201 / 200 running), `text` pasted when ready |
| GET | `/api/sessions/{id}/worktree` | path, branch, base, dirty / untracked / unmerged, removal `refusal` |
| GET | `/api/worktrees/plan?folder=&task=&base=` | `{"plan"\|null,"error"}` |
| GET | `/api/sessions/{id}/ws` | terminal WebSocket (below) |
| GET | `/api/kinds` | session kinds, each `available` |
| GET | `/api/claude-accounts` | `{accounts:[{name,updated}]}` (never tokens); `POST /api/sessions` `"account":NAME` launches a claude session under one |
| POST | `/api/claude-accounts` | `{name,token,replace?}` -> 201 `{name,updated}` (taken: 409 unless `replace`) |
| DELETE | `/api/claude-accounts/{name}` | 204 |
| GET | `/api/workspaces/{id}/conversations` | `?kind=claude\|codex&folder?&limit?` (1-200, default 30): past conversations of the folder, newest first, `{id,kind,modified,title?,prompt?,bytes,open_in?}`; `POST /api/sessions` `"resume":ID\|"latest"` resumes one |
| GET | `/api/usage` | hub and per-workspace `today` / `week` / `total` |
| GET | `/api/search?q=[&regex=&case=&ws=&session=&kind=&source=&limit=&per_session=&offset=]` | hits grouped per session; `truncated`, `partial` ([hub-internals](internals.md#search)) |

A session lists `agent` status (`detail`, `since`, AskUserQuestion's `questions`), `agent_session_id`, `launch`, `usage`, `worktree`, `git`, `pr`, `queue`, `recording`, `live` (`epoch`), pairings.

## Review, handoff, peer review

| Method | Path | |
|---|---|---|
| GET | `/api/sessions/{id}/review[?base=]` | branch, base, ahead / behind, commits, file sets by scope |
| GET | `/api/sessions/{id}/review/diff?scope=committed\|staged\|unstaged\|untracked&path=[&old_path=&offset=&base=]` | one file's diff, paged |
| POST | `/api/sessions/{id}/review/commit` | `message`, `exclude`? (paths, `dir/`; default: the ticked suspects) |
| POST | `/api/sessions/{id}/review/push` | `remote` |
| POST | `/api/sessions/{id}/review/pr` | `title`, `body`, `base`, `draft` |
| POST | `/api/sessions/{id}/review/rebase` | |
| POST | `/api/sessions/{id}/review/abort` | rebase `--abort` |
| POST | `/api/sessions/{id}/review/merge` | |
| GET | `/api/sessions/{id}/pr[?refresh=1]` | branch, PR, checks |
| GET | `/api/sessions/{id}/pr/log?check=` | the failed log's redacted tail |
| POST | `/api/sessions/{id}/pr/fix` | `{"check"?,"dry_run"?}` -> `delivery` `pasted` / `queued` |
| POST | `/api/sessions/{id}/pr/rerun` | two-step |
| POST | `/api/sessions/{id}/pr/ready` | two-step |
| POST | `/api/sessions/{id}/pr/merge` | two-step; plan `forcible`: send `force` (the PR number) |
| POST | `/api/sessions/{id}/review/ask` | `{"kind":"commit_message\|resolve_conflicts","dry_run"?}` |
| POST | `/api/sessions/{id}/review/reviewer` | `{"kind"?,"base"?,"model"?,"dry_run"?}`: 201 launched, 200 asked again; 409 nothing |
| GET | `/api/sessions/{id}/review/findings` | the reviewer's latest reply (409 none) |
| POST | `/api/sessions/{id}/review/findings/send` | `{"prompt"?}` |
| POST | `/api/sessions/{id}/handoff/compose` | `{"source":{"kind":"selection\|transcript\|command\|last_command\|output\|diff",...},"max_bytes"?}` -> `{"prompt"}` |
| POST | `/api/sessions/{id}/handoff` | `{"prompt","from"?,"source"?}`: pasted + Enter (128 KiB) |

**Two-step**: `{"dry_run":true}` -> a plan (summary, exact `commands`, warnings, `refusal`, `needs_rebase`, `fingerprint`; commit adds `suspects`), then the body with `"confirm":fingerprint` runs it -> `{"ok","ran","url"?}`; 409 refused / changed since.

## Fan-outs and Flywheel

| Method | Path | |
|---|---|---|
| GET / POST | `/api/fanouts` | list / launch `{"workspace_id","prompt","candidates":[{"kind","model"?,...}],"graders":[CMD\|{"run","name"?}],"base"?,"protect"?,"from_task"?,...}` (`fanout new`'s flags) -> 201 |
| GET / DELETE | `/api/fanouts/{id}` | the ranked board / forget it (204; sessions, worktrees stay) |
| POST | `/api/fanouts/{id}/grade` | `{"candidate"}` -> 202 |
| POST | `/api/fanouts/{id}/fix` | `{"candidate","dry_run"?}` |
| POST | `/api/fanouts/{id}/pick` | `{"candidate","include_suspects"?}`, two-step (plan adds `merge`, `losers`) |
| POST | `/api/fanouts/{id}/rebase` | `{"candidate"}`, two-step |
| POST | `/api/fanouts/{id}/seen` | `{"graded_at"}` -> 204 (`needs_you` false) |
| POST | `/api/fanouts/{id}/cleanup` | `{"candidates"?,"remove_worktrees"?,"force"?,"dry_run"?}` |
| POST | `/api/fanouts/{id}/promote` | `{"task_id"?,"goal"?,...}`, two-step (plan: path, contents); 409 exists / changed |
| GET | `/api/flywheel` | per adopted folder: counts, `plans`, alerts, live runs, worker |
| GET | `/api/flywheel/panel?folder=` | panel (`queued_in`: a scoped worker runs it; live `last_tool`); 404 not adopted |
| GET | `/api/flywheel/run?folder=&run=&since=N` | `{"status","active","entries","next","attempts"}` (poll `since=next`); 503 read waits |
| GET | `/api/flywheel/history?folder=&task=` | every run; attempts' `outcome`/`error`/`checks` (= `fw show`); task `checks` (held-out: names) |
| GET | `/api/flywheel/task?folder=&task=` | task as a fan-out seed |
| POST | `/api/flywheel/tasks` | `{"folder","task_id","goal","phase"?,"prerequisites"?,"checks":[{"run","name"?}]}`, two-step; 400 invalid; peers 403 |
| GET | `/api/flywheel/autopilot?folder=` | `intake` (+ 20 newest decisions), this hub's autopilot sessions |
| POST | `/api/flywheel/autopilot/start` | `{"folder","confirm":true}` -> 201 the `fw autopilot` session; 400 unconfirmed; 409 running here or elsewhere |
| POST | `/api/flywheel/autopilot/stop` | `{"folder"}` -> 202 (Ctrl-C); 409 none of this hub's |
| POST | `/api/flywheel/autopilot/pause` | `{"folder"}` -> `{"paused","changed"}` |
| POST | `/api/flywheel/autopilot/resume` | as `pause` |
| POST | `/api/flywheel/autopilot/decide` | `{"folder","run","decision":"approve"/"deny","confirm":true}` + `gate`, `attempt`, `feedback` (as `fw hub autopilot`); 409 not that gate or one queued; 404 no run |
| POST | `/api/flywheel/steer/say` | `{"folder","run","message"}` -> 201 one `fw say` command; 400 empty; 409 not running/validating; peers 403 |
| POST | `/api/flywheel/steer/interrupt` | `{"folder","run"}`: as `say`, for `fw interrupt` |
| POST | `/api/flywheel/worker` | `{"folder","workspace_id"?,"another"?,"tasks"?,"with_prereqs"?,"cols"?,"rows"?}` -> 201 the `fw worker` session; 409 running, unless `another` |

Fan-outs off: 503.

## Shares and recordings

| Method | Path | |
|---|---|---|
| GET / POST / DELETE | `/api/shares[?session=]` | list / mint `{"session_id","expires_in"?,"name"?}` -> 201 `{"credential","path"}` (once) / revoke all (of `session`) |
| DELETE | `/api/shares/{which}` | id or label; views close (4004) |
| POST | `/api/sessions/{id}/record/start` | 201 started, 200 already; 409 not running |
| POST | `/api/sessions/{id}/record/stop` | 409 not recording |
| GET | `/api/recordings` | `limits`, `total_bytes`, recordings newest first |
| GET / DELETE | `/api/recordings/{name}` | the `.cast` (`application/x-asciicast`) / 204 (409 active) |

## Activity

| Method | Path | |
|---|---|---|
| GET | `/api/activity[?since=&until=&ws=&session=&kind=&limit=&cursor=&machines=none]` | rows newest first, `next` (a `cursor`), `partial`; unknown selector: 404 |
| GET | `/api/activity/digest[?since=&client=&until=&ws=&session=]` | the away digest ([hub-workflows](workflows.md#activity)); `since=last-seen` needs `client` |
| POST | `/api/activity/seen` | `{"client"}` -> 204: its `last-seen` is now |
| GET | `/api/digest` | schedule, `next`, `recent`, `push` host ([hub-workflows](workflows.md#digests)) |
| GET | `/api/digest/preview[?since=]` | a digest, neither sent nor kept |
| POST | `/api/digest/send` | `{"since"?}` -> the digest sent, `delivery` |
| PUT | `/api/digest/config` | `{"times"?,"timezone"?,"channels"?,"quiet_if_empty"?,"rev"?}` (omitted: kept; `timezone:""`: system) -> rewrites hub.toml `[digest]`; 422; 409 (`stale`: `rev` moved on) |
| GET | `/api/digests/{id}` | one of the last 14 |

Times: RFC 3339, unix ms, an age (`90m`), `last-seen`. `ws`, `session`: id, prefix, or name. `kind`: comma list (`review` = `review.*`); beyond plugin events: `turn.finished`, `commands.suggested`, `usage`, `plugin.ran`, `hub.stopped`. A peer or `machines=none`: this hub alone.

## Settings, config, plugins, notify, TLS

| Method | Path | |
|---|---|---|
| GET / PUT | `/api/settings` | `{"rev","values","keys"}`; PUT `{"rev","values"?:{id: value\|null},"keys"?:{action: [keys]\|null}}` (`null` resets); 409 stale; 422 `{"errors"}` |
| POST | `/api/settings/keys/check` | `{"keys"}` -> `{"errors","warnings"}` |
| GET | `/api/config` | as `fw hub config show --json` |
| GET | `/api/plugins` | each plugin: enabled, valid, error, events, actions |
| PUT | `/api/plugins/{name}` | `{"enabled"}` |
| GET | `/api/plugins/{name}/logs[?limit=N]` | runs newest first (max 200) |
| POST | `/api/plugins/{name}/actions/{action}` | `{"session_id"?}`; 409 disabled / no session |
| GET | `/api/notify` | `{"push":null\|{...}}` |
| POST | `/api/notify/test` | 204 sent; 409 push off; 502 failed |
| GET | `/api/tls` | mode, CA fingerprint, names, expiry |
| POST | `/api/tls/reload` | re-read the CA (`tls rotate`), re-issue, drop old-CA links; 409 off |

## Permissions

Behaviour: [hub-workflows](workflows.md#permissions). `?folder=` (absolute, in a workspace) or `?session=` (bodies: same keys); peers 403.

| Method | Path | |
|---|---|---|
| GET | `/api/perms` | sources, rules, a `pending` request's candidate rules |
| GET | `/api/perms/suggest` | `days` 1-30 (14) -> suggestions, `skipped_dangerous` |
| POST | `/api/perms/preview` | `{"rules"}` (Claude) \| `{"patterns"}` (Codex) \| `{"setting","value"}`, `"file"?` -> path, diff, sentence, warning, `base`, `type_to_confirm`? |
| POST | `/api/perms/allow` | preview's body + `"confirm":true`, its `"base"` (409 changed), `"typed"?`, `"gitignore"?`, `"answer"?` |

## Auth, machines, peers

| Method | Path | |
|---|---|---|
| POST | `/api/login-codes` | `{"code","expires_in":300}` for `/login?code=` |
| POST | `/api/token/rotate` | 204 (as `fw hub token --rotate`) |
| GET | `/api/whoami` | `scope` (`full` / `peer`), version, host, `hub_id` |
| GET / POST | `/api/peers` | list / `{"name"}` -> 201 `{"peer","credential"}` |
| DELETE | `/api/peers/{which}` | revoke by id or name |
| DELETE | `/api/peer` | a peer credential revokes itself |
| GET / POST | `/api/machines` | list (link `status`) / `{"name","url","credential"\|"token","fingerprint"?,"replace"?}` -> 201 (200 `replace`) |
| DELETE | `/api/machines/{name}` | also asks it to revoke the peer |
| POST | `/api/machines/{name}/test` | ok, latency, its `whoami` or error |
| any | `/m/{name}/{*rest}` | proxy to machine `name`'s API and sockets |
| GET | `/api/events[?client=ID]` | events WebSocket (below) |

## Pages (no `/api`)

| Method | Path | |
|---|---|---|
| GET | `/` | app or sign-in |
| GET / POST | `/login` | `?code=` / `?token=` / form: cookie, then `/` |
| POST | `/logout` | ends this login, closes its sockets (4002) |
| GET | `/healthz` | `ok` (no credential) |
| GET | `/assets/{*path}` | UI assets (some public) |
| GET | `/manifest.webmanifest` | PWA manifest (public) |
| GET | `/sw.js` | service worker (public) |
| GET | `/replay` | recording player (`#<name>`) |
| GET | `/ca` | CA install page (public; `--tls auto`) |
| GET | `/ca/` | same |
| GET | `/ca/hub-ca.crt` | CA, DER |
| GET | `/ca/hub-ca.pem` | CA, PEM |
| GET | `/ca/hub-ca.mobileconfig` | iOS profile |
| GET | `/ca/probe` | trust probe |
| GET | `/ca/ca.js` | page script |
| GET | `/ca/ca.css` | page style |
| GET | `/share/{credential}` | share-link viewer |
| GET | `/share/{credential}/info` | link, session (`null`: gone) |
| GET | `/share/{credential}/ws` | read-only socket (protocol 1) |
| GET | `/share/{credential}/assets/{*path}` | its assets |
| any | `/share/{credential}/{*rest}` | 403 |

Unreadable queries / login bodies: a 4xx page (JSON if `Accept: application/json`).

## Terminal WebSocket

`/api/sessions/{id}/ws`: `{"type":"snapshot","cols","rows","alt_screen","bytes"}`, then binary (those `bytes`, then output). Clients size to it, replay, fit, send `resize`.

| Server text message | |
|---|---|
| `{"type":"exit","code","signal"}` | process ended |
| `{"type":"reset"}` | fell behind: clear; a screen-only snapshot follows |
| `{"type":"size","cols","rows"}` | another viewer resized it (in order with output) |
| `{"type":"error","message"}` | input refused or dropped |

Client sends `input` `{"data"}`, `resize` `{"cols","rows"}`, `theme` `{"foreground","background","cursor","ansi"}`, or binary input.

**Protocol 2** (`?proto=2`, the web UI's; another `proto`: 400): `lite=1&fps=N` (1-30, 4), `z=1`, `since=S&epoch=E`, `client=KEY` (1-64 `[A-Za-z0-9_-]`).

- `snapshot` adds `seq` (output offset, one per raw byte) and `epoch` (hex stream id; a relaunch starts another).
- `since`/`epoch` still kept (512 KiB): `{"type":"resume","seq":S,"epoch"}`, the missed output, a `size` (lite, over 16 KiB: a frame); else a snapshot with `continued` (`false`: another stream).
- `{"type":"frame","seq","cols","rows","bytes"}` + one binary repainting what changed.
- Lite: at most `fps` updates/s (raw when <= 1 KiB, else a frame); a raw viewer 64+ chunks behind gets one update.
- `z=1`: binary = `0` + bytes, or `1` + u32 BE length + raw deflate (one stream per socket, sync-flushed).
- `input` may carry `id` (growing per `client`): `{"type":"ack","id"}` once taken (never typed twice); `{"type":"nack","id","retry"}` refused (`retry`: mid-handoff, resend).
- `{"type":"mode","lite","fps"?,"paused"?}` switches pacing (`paused`: nothing until unpaused). `{"type":"ping"}` -> `{"type":"pong"}`.

Close codes: **4001** token rotated, **4002** logout / login expired, **4003** peer revoked, **4004** share ended, **4005** relaunched, **1012** live handoff (both: reconnect). Machine proxy: **4010** (its 4001-4003), **4011** removed, **1011** link lost.

## Events WebSocket

`/api/events[?client=ID]` (as `client` above; default its own). Server sends `{"type",...}`:

| `type` | Other fields | When |
|---|---|---|
| `snapshot` | `"sessions":{id:status},"workspaces":{id:rollup},"graded":[id],"fanouts":{ws:n}` | on connect, falling behind, a fan-out change |
| `status` | `"session_id","status"\|null,"workspace_id","workspace_status"` | per change, as this client sees it |
| `sessions` | | sessions, workspaces, viewers changed: re-list (250 ms apart at most) |
| `usage` | the session's usage | changed |
| `commands` | `"session_id","fresh","commands"` | an agent suggested blocks (`fresh`: within 2 min) |
| `replies` | `"session_id","replies"` | an agent replied (a known `seq` replaces that reply) |
| `flywheel` / `flywheel_reset` | `"folder","summary"\|null` / | a panel changed (`null`: unwatched) / missed some |
| `layout` | `"workspace_id","rev"` | a layout saved or pruned |
| `fanout` | `"id"` | a fan-out changed |
| `settings` / `config` | `"rev"` | UI settings / hub.toml re-read |
| `plugins` / `plugin_toast` | / `"plugin","message","tone"` | plugins changed / a plugin's toast |
| `activity` | `"row"` | a journal row was written |
| `away` | `"since","until"` | back after 10+ min: its digest span |
| `digest` | `"id"` | a digest was sent (`GET /api/digests/{id}`) |

Client sends `{"type":"view","session_id","visible"?:[ids],"present"?}`: what is on screen counts as viewed; `present`: someone is at the page.

## Plugins

Manifest: [hub-reference: plugin.toml](reference.md#plugintoml). A run:

- **Run**: cwd the plugin dir, own process group; stdin `{"version":1,"type":"event"|"action","plugin","event"|"action","at","hub","payload"}`; env an allowlist (`PATH HOME USER LANG ...`) + `FW_HUB_PLUGIN`, `FW_HUB_PLUGIN_DIR`, `FW_HUB_PLUGIN_TRIGGER` (`event:<name>` / `action:<id>`), `FW_HUB_STATE_DIR`, never the token.
- **Limits**: SIGTERM at `timeout_secs`, SIGKILL 0.5 s on; 64 KiB output kept; `plugins.max_concurrent` at once; past 64 waiting, events drop. Logs: `plugin-logs/<name>.jsonl` (200 runs past 512 KiB).
- **Response** (stdout JSON): `{"toast": "text" | {"message","tone":"info|ok|warn|error"}}`; actions also `{"paste":{"text","run"?,"session"?}}` (64 KiB).

### Plugin events

`payload.session` (if any): id, name, kind, state, workspace, cwd, worktree, branch, agent. Rows name main fields; full payloads: log a plugin's stdin.

| Event | Payload adds |
|---|---|
| `hub.started` | - |
| `session.created` | - |
| `session.exited` | `exit`, `hub_stopping` (`fw hub stop` hung it up) |
| `session.removed` | - |
| `status.changed` | `status`, `previous`, `detail`; each finished turn: one `done` (`seen`: watched). Deprecated (gone in 0.8): `turn_finished`, true on the turn's status (watched: the `idle` after it) |
| `review.committed` | `dir`, `head`, `branch`, `base`, `url`, `summary` (every `review.*`) |
| `review.pushed` | as above |
| `review.pr_opened` | as above |
| `review.rebased` | as above |
| `review.merged` | as above |
| `review.requested` | `author`, `reviewer`, `reused`, `kind`, `base` |
| `broadcast.sent` | `broadcast` (`id`, `text` to 2000 chars, `counts`), `workspace` (one, or null), `targets[]` (`session`, `result`, `why`) |
| `handoff.sent` | `from`, `to`, `source` (findings: `review_findings`), `bytes`, `truncated`; target's hub |
| `fanout.finished` | `fanout` once settled (no agent working, nothing queued): candidates best first with `verdict`, `rank`, `tail` (last 400 stderr chars of the first failing grader); no `session` |
| `queue.delivered` | `item` (`text` to 2000 chars), `pending` |
| `queue.stalled` | `reason`, `item` (the next, or null), `pending` |
| `share.created` | `share` (no credential) |
| `share.revoked` | as above, per link revoked (not on expiry) |
| `record.started` | `recording` (no path); `session` `null` once removed |
| `perms.allowed` | `agent`, `file`, `rules` (as written), `folder`, `dangerous` |
| `pr.checks_failed` | `dir`, `branch`, `pr`, `checks`, `failing` (every `pr.*`) |
| `pr.checks_passed` | as above |
| `pr.review_changes_requested` | as above |
| `pr.merged` | as above |
| `record.stopped` | + `reason` `stopped` / `ended` (exit, write failure); not on hub stop, handoff |
| `autopilot.started` | `folder`, `name`, `pid`; no `session` (all `autopilot.*`: `folder`, `name`) |
| `autopilot.stopped` | + `state` |
| `autopilot.decision` | `cycle`, `emitted`, `dropped`, `reason`, `tasks`, tier lists; once per deciding cycle |
| `autopilot.approval_needed` | `task_id`, `run_id`, `instruction`, `gate`, `attempt` (once per gate, attempt) |
| `autopilot.approved` | `task_id`, `run_id`, `gate`, `attempt`, `command_id`, `feedback` (bool, not the text) |
| `autopilot.denied` | as `approved` |
| `digest.ready` | `digest`: as `GET /api/digests/{id}`; no `session` |

## Prompt queues

Behaviour: [hub-workflows](workflows.md#prompt-queue). `{item}`: id prefix (4+) or position (1 = next). Writes return the queue.

| Method | Path | |
|---|---|---|
| GET | `/api/queues` | every session's queue |
| GET / POST | `/api/sessions/{id}/queue` | `paused`, `stalled`, `items`, `waiting` (why the next waits; 400 a shell); POST `{"text","when"?}` -> 201 |
| PATCH / DELETE | `/api/sessions/{id}/queue/items/{item}` | PATCH `{"text"?,"when"?}`; 409 one being sent |
| POST | `/api/sessions/{id}/queue/items/{item}/move` | `{"position"}` (1-based among pending) |
| POST | `/api/sessions/{id}/queue/items/{item}/editing` | `{"editing"}` -> 204: an editor holds it (renew in 30 s) or lets go |
| POST | `/api/sessions/{id}/queue/pause` | |
| POST | `/api/sessions/{id}/queue/resume` | also clears a stall |
| POST | `/api/sessions/{id}/queue/clear` | `{"all"?}`: the delivered history too |
| POST | `/api/broadcast` | `{"targets","text","header"?,"recipients"?}` (header names) -> `counts`, `results[]` (`result` `sent`/`queued`/`refused`, `why`) |

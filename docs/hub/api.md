# Hub API

REST, WebSockets, events, plugin payloads; `docs_drift.rs` checks this page lists exactly the mounted routes. Shapes name the main fields; `curl` for the rest. Front door: [hub](../hub.md).

## Conventions

- JSON under `/api`; every route needs `Authorization: Bearer <token>`, a login cookie, or a peer credential.
- Errors `{"error"}`: 400 malformed, 404 unknown, 405 method, 409 refused by state, 422 wrong JSON shape.
- Cross-origin `Origin` is refused on upgrades and non-GET requests; peers get 403 on the routes in [hub-security](security.md#machines-and-peers).
- `cols`/`rows` of 0 mean the default (120x32).

```bash
T=$(fw hub token)
curl -s -H "Authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"name":"flywheel","folders":["/home/me/code/flywheel"]}' http://127.0.0.1:7420/api/workspaces
```

## Workspaces

| Method | Path | |
|---|---|---|
| GET / POST | `/api/workspaces` | list / create `{"name","folders":[...]}`; each lists its `templates` |
| GET / PATCH / DELETE | `/api/workspaces/{id}` | PATCH `name` / `folders` (409 if it drops a running session's folder); DELETE 409 while sessions run |
| GET / PUT | `/api/workspaces/{id}/layout` | `{"rev","tabs":[{"id","name","root"}]}`; node `{"type":"pane","session"}` or `{"type":"split","dir":"right\|down","ratio","a","b"}`. PUT -> `rev`+1; 409 stale; 400 invalid (ratio 0.05..0.95, max 12 tabs / 12 panes) |
| POST | `/api/workspaces/import` | `{"path","name"?,"update"?}` (a `.code-workspace` on the hub's machine): 201 / 200 updated / 409 name taken |
| GET | `/api/code-workspaces[?root=]` | `.code-workspace` files under `root` (default `--import-root`) |
| GET | `/api/folders/check?path=` | always 200: `{"ok":true,"path":canonical,"git"}` or `{"ok":false,"error"}` |
| GET | `/api/repos/check` | old `/api/folders/check` |
| GET | `/api/templates[?workspace_id=]` | launch templates |
| POST | `/api/workspaces/{id}/templates` | create; `"replace": true` overwrites the same `name`, else 409; unknown fields refused |
| DELETE | `/api/workspaces/{id}/templates/{name}` | delete a template |

Workspace JSON: `folders: [{"path","name"?,"git","missing"}]` (+ legacy `repos`); input takes paths or `{"path","name"}` (no `name` keeps the label, `""` clears it).

## Sessions

| Method | Path | |
|---|---|---|
| GET / POST | `/api/sessions[?workspace_id=]` | list / create `{"workspace_id","folder"?,"kind"?,"command"?:[argv],"worktree"?:{"task","base"?},"prompt"?,"model"?,...}` (as `fw hub new`; missing CLI or dir 400) |
| GET / PATCH / DELETE | `/api/sessions/{id}` | PATCH `{"name"}`; DELETE kills and forgets (204); `?remove_worktree=true[&force=true]` also the companions + worktree (409 with why) |
| POST | `/api/sessions/{id}/kill` | 202: SIGHUP (SIGKILL after 3 s), record kept; 409 nothing to kill |
| POST | `/api/sessions/{id}/relaunch` | an `exited`/`stopped` session in place (same id, command, folder; fresh PTY); `{"cols"?,"rows"?}`; 409 while it runs |
| POST | `/api/sessions/{id}/restart` | old `/relaunch` |
| GET | `/api/sessions/{id}/screen[?source=visible\|recent&lines=N]` | rendered text (below) |
| GET | `/api/sessions/{id}/explain` | `{"status","source","screen":{"state","rule","evidence"},"manifest":{"rules"}}` (as `fw hub explain`) |
| POST | `/api/sessions/{id}/input` | `{"data","paste"?,"enter"?}` -> `{"bytes","bracketed"}`; 409 no running terminal / not reading |
| POST | `/api/sessions/{id}/paste` | `{"text","run"?}`: `/input` with `paste`, trailing newlines dropped, max 1 MiB; 409 also on unsafe multi-line |
| GET | `/api/sessions/{id}/commands[?limit=N&all=true]` | suggested blocks newest first (shell only unless `all`; `limit` 1..500, default 50): `{"commands":[{"seq","lang","text","command"}]}` |
| GET | `/api/sessions/{id}/transcript[?around=N&before=N&after=N]` | `{"messages":[{"index","role","text"}],"next","complete"}` (30 around `around`, max 200; else the newest); 400 a shell |
| POST | `/api/sessions/{id}/companion` | `{"cols"?,"rows"?,"text"?,"run"?}`: the companion shell (201 started, 200 running); `text` is pasted once it is ready |
| GET | `/api/sessions/{id}/worktree` | `{"path","branch","base","dirty","untracked","unmerged","refusal"}` (runs git) |
| GET | `/api/worktrees/plan?folder=&task=&base=` | `{"plan":{"path","branch","base"}\|null,"error"}` |
| GET | `/api/sessions/{id}/ws` | terminal WebSocket (below) |
| GET | `/api/kinds` | `{"kinds":[{"kind","available"}]}` |
| GET | `/api/usage` | hub and per-workspace `today` / `week` / `total` |
| GET | `/api/search?q=[&regex=&case=&ws=&session=&kind=&source=&limit=&per_session=&offset=]` | below |

Session views carry `agent` (`{"status","detail"?,"since"}`), `agent_session_id`, `launch` (`unchecked` under `bypassPermissions`), `usage`, `worktree`, `companion_of`, `reviewer` / `review_of`, `recording`, `live`, `git`.

**Screen**: `source=visible` (default) = the screen's rows (`lines` refused); `recent` = the last `lines` (1..100000, default 100) logical lines of scrollback + screen, soft wraps joined, trailing blank rows dropped. Both read the active screen.

```json
{"source":"visible","cols":80,"rows":24,"alt_screen":true,"title":"vim","cursor":{"row":2,"col":2,"visible":true},
 "modes":["app_cursor","bracketed_paste","mouse_drag","sgr_mouse"],"lines":["..."]}
```

**Search**: `{"groups":[{"session_id","hits"}],"truncated","partial"}`; `kind`/`source` are comma lists; a hit has `source`, `before`, `match`, `after`, and where it is.

## Review, handoff, peer review

| Method | Path | |
|---|---|---|
| GET | `/api/sessions/{id}/review[?base=]` | `{"branch","base","ahead","behind","commits","committed","staged","unstaged","untracked","conflicts"}` (file sets) |
| GET | `/api/sessions/{id}/review/diff?scope=committed\|staged\|unstaged\|untracked&path=[&old_path=&offset=&base=]` | one file's diff lines, paged by `offset` |
| POST | `/api/sessions/{id}/review/commit` | body `message` |
| POST | `/api/sessions/{id}/review/push` | body `remote` |
| POST | `/api/sessions/{id}/review/pr` | body `title`, `body`, `base`, `draft` |
| POST | `/api/sessions/{id}/review/rebase` | |
| POST | `/api/sessions/{id}/review/abort` | `git rebase --abort` |
| POST | `/api/sessions/{id}/review/merge` | |
| POST | `/api/sessions/{id}/review/ask` | `{"kind":"commit_message\|resolve_conflicts","dry_run"?}` -> `{"agent","prompt","paste"?}` |
| POST | `/api/sessions/{id}/review/reviewer` | `{"kind"?,"base"?,"dry_run"?}`: 201 launched, 200 asked again / dry; 409 nothing to review |
| GET | `/api/sessions/{id}/review/findings` | the reviewer's latest `reply` and its `blocks` (409 no reviewer) |
| POST | `/api/sessions/{id}/review/findings/send` | `{"prompt"?}` -> `{"to","paste"}` |
| POST | `/api/sessions/{id}/handoff/compose` | `{"source":{"kind":"selection\|transcript\|command\|last_command\|diff",...},"max_bytes"?}` -> `{"prompt"}` (sends nothing) |
| POST | `/api/sessions/{id}/handoff` | `{"prompt","from"?,"source"?}` -> `{"to","paste"}`: pasted into agent `id` + Enter (400 a shell / the source; max 128 KiB) |

Review actions (commit, push, pr, rebase, abort, merge) are two-step: `{"dry_run":true}` -> plan `{"summary","commands":[{"dir","display"}],"fingerprint","refusal","needs_rebase"}`, then `{"confirm":fingerprint}` -> `{"ok","ran":[{"display","code","output"}],"url"?}`; 409 refused / stale.

## Fan-outs and Flywheel

| Method | Path | |
|---|---|---|
| GET / POST | `/api/fanouts` | list / launch `{"workspace_id","prompt","candidates":[{"kind","model"?,...}],"graders":[CMD\|{"run","name"?}],"base"?,"protect"?,"from_task"?,...}` -> 201 view |
| GET / DELETE | `/api/fanouts/{id}` | the ranked board / forget it (204; sessions and worktrees stay) |
| POST | `/api/fanouts/{id}/grade` | `{"candidate"}` -> 202 |
| POST | `/api/fanouts/{id}/fix` | `{"candidate","dry_run"?}` -> `{"prompt","paste"?}` |
| POST | `/api/fanouts/{id}/pick` | `{"candidate","dry_run":true}` -> `{"refusal","needs_rebase","merge":plan,"losers"}`, then `{"confirm"}` |
| POST | `/api/fanouts/{id}/rebase` | `{"candidate","dry_run":true}` -> plan, then `{"confirm"}` |
| POST | `/api/fanouts/{id}/seen` | `{"graded_at"}` -> 204: that grading was looked at (`needs_you` false unless a later one finished) |
| POST | `/api/fanouts/{id}/cleanup` | `{"candidates"?,"remove_worktrees"?,"force"?,"dry_run"?}` -> per-candidate `items` |
| POST | `/api/fanouts/{id}/promote` | `{"task_id"?,"goal"?,...,"dry_run":true}` -> `{"plan":{"path","contents"},"fingerprint"}`, then `{"confirm"}`; 404 not a flywheel repo, 409 exists / changed |
| GET | `/api/flywheel` | `{"enabled","folders":[summary]}`; summary `{"folder","counts":{state:n},"alerts","live_runs","worker_live","problem"}` |
| GET | `/api/flywheel/panel?folder=` | the whole panel; 404 unless an adopted workspace folder |
| GET | `/api/flywheel/run?folder=&run=&since=N` | `{"status","active","entries","next"}`; poll with `since=next`. 400 bad id, 404 unknown, 503 while an earlier read waits on the store |
| GET | `/api/flywheel/task?folder=&task=` | the task as a fan-out seed (`prompt`, `graders`, `held_out` names) |
| POST | `/api/flywheel/worker` | `{"folder","workspace_id"?,"another"?,"tasks"?,"with_prereqs"?,"cols"?,"rows"?}` -> 201 the `fw worker` session (`--task` per id in `tasks`, 400 bad id); 409 `{"running"}` while a hub-started worker runs there, unless `another` |

Fan-out view: `{"id","base_sha","winner","counts","leader","settled","graded_at","needs_you","candidates":[{"label","session_id","verdict","rank","why","pickable"}]}` (lists carry no grader output). 503 fan-outs off.

## Shares and recordings

| Method | Path | |
|---|---|---|
| GET / POST / DELETE | `/api/shares[?session=]` | list / mint `{"session_id","expires_in"?,"name"?}` -> 201 `{"credential","path"}` (shown once) / revoke all (of `session`) |
| DELETE | `/api/shares/{which}` | id or label -> `{"revoked"}`; open views close (4004) |
| POST | `/api/sessions/{id}/record/start` | `{"recording"}`: 201 started, 200 already; 409 not running |
| POST | `/api/sessions/{id}/record/stop` | as start; 409 not recording |
| GET | `/api/recordings` | `{"limits","total_bytes","recordings"}` newest first, parts in order |
| GET / DELETE | `/api/recordings/{name}` | the `.cast` (`application/x-asciicast`, attachment) / 204 (409 while active) |

## Settings, config, plugins, notify, TLS

| Method | Path | |
|---|---|---|
| GET / PUT | `/api/settings` | GET `{"rev","values","keys"}`; PUT `{"rev","values"?:{id: value\|null},"keys"?:{action: [keys]\|null}}`; 409 stale; 422 `{"errors"}` |
| POST | `/api/settings/keys/check` | `{"keys":{action:[keys]}}` -> `{"errors","warnings"}` (saves nothing) |
| GET | `/api/config` | as `fw hub config show --json` |
| GET | `/api/plugins` | `{"enabled","plugins":[{"name","enabled","valid","error","events","actions"}]}` |
| PUT | `/api/plugins/{name}` | `{"enabled"}` -> the plugin |
| GET | `/api/plugins/{name}/logs[?limit=N]` | `{"runs"}` newest first (max 200) |
| POST | `/api/plugins/{name}/actions/{action}` | `{"session_id"?}` -> `{"ok","toast","paste","log"}`; 404 unknown, 409 disabled / off / needs a session |
| GET | `/api/notify` | `{"push":null\|{"host","events",...}}` |
| POST | `/api/notify/test` | 204 sent; 409 push off; 502 refused / unreachable |
| GET | `/api/tls` | `{"mode","ca_fingerprint","constraints","names","expires"}` |
| POST | `/api/tls/reload` | re-read the CA (`fw hub tls rotate` calls it), re-issue, close old-CA TLS connections; 409 TLS off |

## Auth, machines, peers

| Method | Path | |
|---|---|---|
| POST | `/api/login-codes` | `{"code","expires_in":300}` for `/login?code=` |
| POST | `/api/token/rotate` | 204 (as `fw hub token --rotate`) |
| GET | `/api/whoami` | `{"scope":"full"\|"peer","peer","version","host","hub_id"}` |
| GET / POST | `/api/peers` | `{"peers":[{"id","name","last_used"}]}` / `{"name"}` -> 201 `{"peer","credential"}` |
| DELETE | `/api/peers/{which}` | revoke by id or name |
| DELETE | `/api/peer` | a peer credential revokes itself |
| GET / POST | `/api/machines` | list / `{"name","url","credential"\|"token","fingerprint"?,"replace"?}` -> 201 (200 on `replace`); views carry link `status` |
| DELETE | `/api/machines/{name}` | `{"machine","revoked","note"}` |
| POST | `/api/machines/{name}/test` | `{"ok","latency_ms","whoami"\|"error"}` |
| any | `/m/{name}/{*rest}` | proxy to machine `name`'s `/api/...` ([hub-security: Machines and peers](security.md#machines-and-peers)) |
| GET | `/api/events[?client=ID]` | events WebSocket (below) |

## Pages (no `/api`)

| Method | Path | |
|---|---|---|
| GET | `/` | the app, or the sign-in form |
| GET / POST | `/login` | `?code=` / `?token=` / the form: sets the cookie, then `/` (keeps `#session=`) |
| POST | `/logout` | ends this login, closes its sockets (4002) |
| GET | `/healthz` | `ok` (no credential) |
| GET | `/assets/{*path}` | UI assets (fonts, icons, `login.js`: public) |
| GET | `/manifest.webmanifest` | PWA manifest (public) |
| GET | `/sw.js` | service worker (public) |
| GET | `/replay` | recording player (`/replay#<name>`; data needs a sign-in) |
| GET | `/ca` | local-CA install page (public; 404 without `--tls auto`) |
| GET | `/ca/` | same |
| GET | `/ca/hub-ca.crt` | CA, DER |
| GET | `/ca/hub-ca.pem` | CA, PEM |
| GET | `/ca/hub-ca.mobileconfig` | iOS profile |
| GET | `/ca/probe` | https probe: does this device trust the CA |
| GET | `/ca/ca.js` | page script |
| GET | `/ca/ca.css` | page style |
| GET | `/share/{credential}` | share-link viewer |
| GET | `/share/{credential}/info` | the link and its session (`null` once gone) |
| GET | `/share/{credential}/ws` | read-only terminal socket |
| GET | `/share/{credential}/assets/{*path}` | the viewer's assets |
| any | `/share/{credential}/{*rest}` | 403 |

Unreadable queries (or `POST /login` bodies) get a 4xx page, or `{"error"}` for `Accept: application/json`.

## Terminal WebSocket

`/api/sessions/{id}/ws`: `{"type":"snapshot","cols","rows","alt_screen","bytes"}`, then binary frames (`bytes` of snapshot, then live output). Clients size to `cols`x`rows`, replay, fit, send `resize`.

| Server text message | |
|---|---|
| `{"type":"exit","code","signal"}` | process ended |
| `{"type":"reset"}` | fell behind: clear; a screen-only snapshot follows |
| `{"type":"size","cols","rows"}` | another viewer resized it (in order with output) |
| `{"type":"error","message"}` | input refused or dropped |

Client sends `input` `{"data"}`, `resize` `{"cols","rows"}`, `theme` `{"foreground","background","cursor","ansi"}` (answers colour queries), or binary input.

Close codes: **4001** token rotated, **4002** logout / login expired, **4003** peer revoked, **4004** share ended, **1012** live handoff (reconnect). Via a machine proxy: **4010** (its 4001-4003), **4011** machine removed, **1011** link lost.

## Events WebSocket

`/api/events[?client=ID]` (`client`: 1-64 of `[A-Za-z0-9_-]`; omitted: its own). Server sends:

| Message | When |
|---|---|
| `{"type":"snapshot","sessions":{id:status},"workspaces":{id:rollup},"graded":[id],"fanouts":{workspace:n}}` | on connect, after falling behind, and after a fan-out change |
| `{"type":"status","session_id","status"\|null,"workspace_id","workspace_status"}` | per change, as this client sees it |
| `{"type":"sessions"}` | sessions, workspaces, or attached sockets (`live.clients`) changed: re-list |
| `{"type":"usage", ...}` | a session's usage changed |
| `{"type":"commands","session_id","fresh","commands"}` | an agent suggested code blocks (`fresh`: written in the last 2 min) |
| `{"type":"flywheel","folder","summary"\|null}` / `{"type":"flywheel_reset"}` | a panel changed (`null`: unwatched) / some missed: re-list |
| `{"type":"layout","workspace_id","rev"}` | a layout saved or pruned |
| `{"type":"fanout","id"}` | a fan-out changed |
| `{"type":"settings","rev"}` / `{"type":"config","rev"}` | UI settings / hub.toml re-read |
| `{"type":"plugins"}` / `{"type":"plugin_toast","plugin","message","tone"}` | plugins changed / a plugin's toast |

`sessions` is paced to one per 250 ms (repeats fold, never dropped). Client sends `view` `{"session_id","visible"?:[ids]}`: the sessions on screen count as viewed (none while hidden).

## Plugins

Manifest: [hub-reference: plugin.toml](reference.md#plugintoml). A run:

- **Process**: cwd = the plugin dir, own process group, stdin = `{"version":1,"type":"event"|"action","plugin","event"|"action","at","hub","payload"}`.
- **Environment**: an allowlist (`PATH HOME USER SHELL LANG TZ TMPDIR TERM XDG_* DISPLAY ...`) plus `FW_HUB_PLUGIN`, `FW_HUB_PLUGIN_DIR`, `FW_HUB_PLUGIN_TRIGGER` (`event:<name>` / `action:<id>`), `FW_HUB_STATE_DIR`; never the token.
- **Limits**: group SIGTERM at `timeout_secs`, SIGKILL 0.5 s later; 64 KiB of output kept; `plugins.max_concurrent` at once; past 64 waiting, events drop.
- **Response** (stdout JSON): `{"toast": "text" | {"message","tone":"info|ok|warn|error"}}`; actions may return `{"paste":{"text","run"?,"session"?}}` (64 KiB). Other stdout is logged.
- **Logs**: `plugin-logs/<name>.jsonl` (200 runs past 512 KiB); failures also reach the hub log.

### Plugin events

`payload.session` (when there is one): `id`, `name`, `kind`, `state`, `workspace`, `workspace_id`, `cwd`, `worktree`, `branch`, `agent`.

| Event | Payload adds |
|---|---|
| `hub.started` | - |
| `session.created` | - |
| `session.exited` | `exit` |
| `session.removed` | - |
| `status.changed` | `status`, `previous`, `detail` |
| `review.committed` | `dir`, `head`, `branch`, `base`, `url`, `summary` (every `review.*`) |
| `review.pushed` | as above |
| `review.pr_opened` | as above |
| `review.rebased` | as above |
| `review.merged` | as above |
| `review.requested` | `author`, `reviewer`, `reused`, `kind`, `base`, `branch`, `dir` |
| `handoff.sent` | `from`, `to`, `source`, `bytes`, `truncated` (on the target's hub; findings: `source` `review_findings`) |
| `fanout.finished` | `fanout` (below); no `session` |
| `queue.delivered` | `item` `{"id","text" (first 2000 chars),"bytes","when","added_at","delivered_at"}`, `pending` |
| `queue.stalled` | `reason`, `item` (the next, `delivered_at` null; or null), `pending` |
| `share.created` | `share` `{"id","session_id","name","created","expires"}` (unix s); never the credential, digest, or link |
| `share.revoked` | as above, one per link a revoke ends; expiry sends none |
| `record.started` | `recording` (as `/record/start`, no path); `session` `null` once removed |
| `record.stopped` | as above plus `reason` `stopped` (by hand) or `ended` (exit, write failure); hub stop and handoff send none |

`fanout.finished`: once each time a grading leaves a fan-out settled (no agent working, nothing queued):

```json
{"fanout":{"id","name","workspace","base_sha","leader","winner","counts":{"candidates","passing","failing"},
  "candidates":[{"label","rank","verdict":"pass|fail|stale|pending|none","kind","model","branch",
    "graders_passed","graders_total","diff","tail","cost_usd","why","pickable"}]}}
```

Candidates best first; `tail` = the last 400 characters of the first failing grader's stderr, no other output.

## Prompt queues

Behaviour: [hub-workflows: Prompt queue](workflows.md#prompt-queue). `{item}` = an id prefix (4+) or a position (1 = next). Writes return the queue.

| Method | Path | |
|---|---|---|
| GET | `/api/queues` | every session's queue |
| GET / POST | `/api/sessions/{id}/queue` | GET `{"paused","stalled"?,"items","waiting"?}` (400 a shell); POST `{"text","when"?}` -> 201 `{"item","queue"}` |
| PATCH / DELETE | `/api/sessions/{id}/queue/items/{item}` | PATCH `{"text"?,"when"?}`; 409 one being sent |
| POST | `/api/sessions/{id}/queue/items/{item}/move` | `{"position"}` (1-based among pending) |
| POST | `/api/sessions/{id}/queue/items/{item}/editing` | `{"editing"}`: an editor holds it (renew within 30 s) or let go -> 204 |
| POST | `/api/sessions/{id}/queue/pause` | |
| POST | `/api/sessions/{id}/queue/resume` | also clears a stall |
| POST | `/api/sessions/{id}/queue/clear` | `{"all"?}`: the delivered history too |

Item: `{"id","text","when","state":"pending|sending|delivered","delivered_at"?,"reconciled"?}`; `waiting`: why the next is not sent now. Session listings carry a `queue` summary.

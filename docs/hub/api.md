# Hub API

REST, WebSockets, events, plugin payloads. `crates/flywheel-hub/src/docs_drift.rs` checks this page lists exactly the routes the server mounts. Shapes below name the main fields; `curl` a route for the full one. Front door: [hub](../hub.md).

## Conventions

- JSON under `/api`; every route needs `Authorization: Bearer <token>`, a login cookie, or a peer credential.
- Errors are `{"error":"..."}`: 400 malformed body or query, 404 unknown, 405 wrong method, 409 refused by state, 422 wrong JSON shape.
- Cross-origin `Origin` is refused on WebSocket upgrades and every non-GET request. Peer credentials get 403 on the routes in [hub-security](security.md#machines-and-peers).
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
| GET / PATCH / DELETE | `/api/workspaces/{id}` | PATCH `name` and/or `folders` (409 if it drops a running session's folder); DELETE refuses (409) while sessions run |
| GET / PUT | `/api/workspaces/{id}/layout` | `{"workspace_id","rev","tabs":[{"id","name","root"}]}`; node `{"type":"pane","session"}` or `{"type":"split","dir":"right\|down","ratio","a","b"}`. PUT `{"rev","tabs"}` -> `rev`+1; 409 stale; 400 invalid (ratio 0.05..0.95, max 12 tabs / 12 panes) |
| POST | `/api/workspaces/import` | `{"path","name"?,"update"?}` (a `.code-workspace` on the hub's machine) -> `{"workspace","created","file","skipped":[{"entry","path"?,"reason"}]}`; 201 created, 200 updated, 409 name taken without `update` |
| GET | `/api/code-workspaces[?root=]` | `{"root","depth","files":[{"path","name","folders","paths","skipped","workspace_id"}\|{"path","name","error"}],"truncated"}` (default root `--import-root`) |
| GET | `/api/folders/check?path=` | always 200: `{"ok":true,"path":canonical,"git"}` or `{"ok":false,"error"}` |
| GET | `/api/repos/check` | old name of `/api/folders/check` |
| GET | `/api/templates[?workspace_id=]` | launch templates |
| POST | `/api/workspaces/{id}/templates` | create; `"replace": true` overwrites the same `name`, else 409; unknown fields refused |
| DELETE | `/api/workspaces/{id}/templates/{name}` | delete a template |

Workspace JSON: `folders: [{"path","name"?,"git","missing"}]` plus a legacy `repos` mirror of every path; input takes `folders` (paths or `{"path","name"}`; a path without `name` keeps its label, `""` clears it) or the legacy `repos`.

## Sessions

| Method | Path | |
|---|---|---|
| GET / POST | `/api/sessions[?workspace_id=]` | list / create `{"workspace_id","folder"?,"name"?,"kind"?,"command"?:[argv],"worktree"?:{"task","base"?},"prompt"?,"model"?,"permission_mode"?,"approval"?,"sandbox"?}` (`kind` default `shell`; missing CLI or dir 400) |
| GET / PATCH / DELETE | `/api/sessions/{id}` | PATCH `{"name"}`; DELETE kills and forgets (204); `?remove_worktree=true[&force=true]` also removes companions + worktree: 200 `{"removed":[ids],"worktree":{"path","branch","branch_deleted"}}`, 409 with why |
| POST | `/api/sessions/{id}/kill` | 202: SIGHUP the session (SIGKILL after 3 s); record kept. 409 nothing left to kill |
| POST | `/api/sessions/{id}/relaunch` | an ended (`exited`/`stopped`) session in place: same id, name, command, folder; fresh PTY. Body `{"cols"?,"rows"?}`. 409 while it runs |
| POST | `/api/sessions/{id}/restart` | old name of `/relaunch` |
| GET | `/api/sessions/{id}/screen[?source=visible\|recent&lines=N]` | rendered text (below) |
| GET | `/api/sessions/{id}/explain` | `{"status","source":{"kind":"hook\|manifest\|heuristic\|hub\|none"},"screen":{"state","rule","evidence"},"manifest":{"file","version","rules":[{"id","matched","reason"}]},"problems"}` |
| POST | `/api/sessions/{id}/input` | `{"data","paste"?,"enter"?}` -> `{"bytes","bracketed"}` (`paste`: as `fw hub send`). 409 no running terminal / not reading |
| POST | `/api/sessions/{id}/paste` | `{"text","run"?}` -> `{"bracketed","ran","bytes"}`: as `/input` with `paste`, plus trailing newlines dropped, at most 1 MiB, whitespace-only 400; 409 not running, not reading, or unsafe multi-line |
| GET | `/api/sessions/{id}/commands[?limit=N&all=true]` | suggested blocks newest first (shell only unless `all`; `limit` 1..500, default 50): `{"session_id","conversation","transcript","commands":[{"seq","conversation","message_id","timestamp","block_index","lang","shell","text","command","source"}]}` |
| GET | `/api/sessions/{id}/transcript[?around=N&before=N&after=N]` | `{"messages":[{"index","role","kind","tool"?,"timestamp","text"}],"first","next","complete"}`: `before`/`after` around `around` (default 30, max 200; else the newest); 400 for a shell |
| POST | `/api/sessions/{id}/companion` | `{"cols"?,"rows"?,"text"?,"run"?}`: the agent's companion shell (201 started, 200 running); with `text` also pastes once ready: `{"session","created","ready":"running\|bracketed\|quiet\|timeout","paste"}` |
| GET | `/api/sessions/{id}/worktree` | `{"worktree","path","branch","base","missing","dirty","untracked","unmerged","refusal","companions","blocking"}` (runs git) |
| GET | `/api/worktrees/plan?folder=&task=&base=` | `{"dir","default_base","branches","plan":{"path","branch","base"}\|null,"error"}` |
| GET | `/api/sessions/{id}/ws` | terminal WebSocket (below) |
| GET | `/api/kinds` | `{"kinds":[{"kind","available","program"}]}` |
| GET | `/api/usage` | hub and per-workspace `today` / `week` / `total` |
| GET | `/api/search?q=[&regex=&case=&ws=&session=&kind=&source=&limit=&per_session=&offset=]` | below |

Session views carry `agent` (`{"status","detail"?,"since"}`), `agent_session_id`, `launch` (`unchecked: true` under `bypassPermissions`), `usage`, `worktree`, `companion_of`, `reviewer` / `review_of`, `recording`, `live.clients`, `live.link_viewers`, and `git` (`{"branch","dirty","untracked","ahead","behind"}`).

**Screen text**: `source=visible` (default) is the screen's rows (`lines` refused); `recent` the last `lines` (1..100000, default 100) logical lines of scrollback + screen, soft wraps joined, trailing blank rows dropped. Both read the active screen.

```json
{"source":"visible","cols":80,"rows":24,"alt_screen":true,"title":"vim","cursor":{"row":2,"col":2,"visible":true},
 "modes":["app_cursor","bracketed_paste","mouse_drag","sgr_mouse"],"lines":["..."]}
```

**Search**: `{"groups":[{"session_id","session","workspace","counts","more","hits":[hit]}],"hits","truncated","partial","elapsed_ms"}`; `kind`/`source` are comma lists. A hit: `source`, `matches`, `before`, `match`, `after`, plus `from_end` (scrollback), `index` / `message_id` (transcript), or `seq` (commands).

## Review, handoff, peer review

| Method | Path | |
|---|---|---|
| GET | `/api/sessions/{id}/review[?base=]` | `{"branch","head","base":{"name","sha","merge_base"},"ahead","behind","upstream","remotes","commits","committed","staged","unstaged","untracked","conflicts","operation"}`; each set `{"files":[{"path","status","insertions","deletions","binary"}]}` |
| GET | `/api/sessions/{id}/review/diff?scope=committed\|staged\|unstaged\|untracked&path=[&old_path=&offset=&base=]` | `{"path","scope","binary","lines","offset","next","total_lines","truncated"}` |
| POST | `/api/sessions/{id}/review/commit` | body `message` |
| POST | `/api/sessions/{id}/review/push` | body `remote` |
| POST | `/api/sessions/{id}/review/pr` | body `title`, `body`, `base`, `draft` |
| POST | `/api/sessions/{id}/review/rebase` | |
| POST | `/api/sessions/{id}/review/abort` | `git rebase --abort` |
| POST | `/api/sessions/{id}/review/merge` | |
| POST | `/api/sessions/{id}/review/ask` | `{"kind":"commit_message\|resolve_conflicts","dry_run"?}` -> `{"agent","prompt","paste"?}` |
| POST | `/api/sessions/{id}/review/reviewer` | `{"kind"?,"base"?,"dry_run"?}` -> `{"author","reviewer"?,"kind","reused","fallback"?,"flags","base","branch","prompt","diff_bytes","diff_truncated","dry_run"}`: 201 launched, 200 asked again / dry; 409 nothing to review |
| GET | `/api/sessions/{id}/review/findings` | `{"author","reviewer":{"id","name","kind","running","status"?},"reply"?,"timestamp"?,"blocks":[{"lang","shell","text","command"}],"prompt"?,"truncated"}` (409 no reviewer) |
| POST | `/api/sessions/{id}/review/findings/send` | `{"prompt"?}` -> `{"to","paste"}` |
| POST | `/api/sessions/{id}/handoff/compose` | `{"source":{"kind":"selection\|transcript\|command\|last_command\|diff",...},"max_bytes"?}` -> `{"prompt","content_bytes","kept_bytes","truncated"}` (sends nothing) |
| POST | `/api/sessions/{id}/handoff` | `{"prompt","from"?,"source"?,"truncated"?}` -> `{"to","paste"}`: paste into agent `id`, then Enter (400 a shell / the source itself; at most 128 KiB) |

Review actions (commit, push, pr, rebase, abort, merge) are two-step: `{"dry_run":true,...}` -> plan `{"summary","commands":[{"dir","display"}],"fingerprint","refusal","needs_rebase"}`, then `{"confirm":fingerprint,...}` -> `{"ok","ran":[{"display","code","output"}],"url"?}`; 409 refused / stale.

## Fan-outs and Flywheel

| Method | Path | |
|---|---|---|
| GET / POST | `/api/fanouts` | list / launch `{"workspace_id","prompt","candidates":[{"kind","model"?,...}],"graders":[CMD\|{"run","name"?}],"base"?,"timeout_secs"?,"protect"?,"from_task"?}` -> 201 view |
| GET / DELETE | `/api/fanouts/{id}` | the ranked board / forget it (204; sessions and worktrees stay) |
| POST | `/api/fanouts/{id}/grade` | `{"candidate"}` -> 202 |
| POST | `/api/fanouts/{id}/fix` | `{"candidate","dry_run"?}` -> `{"prompt","paste"?}` |
| POST | `/api/fanouts/{id}/pick` | `{"candidate","dry_run":true}` -> `{"refusal","needs_rebase","merge":plan,"losers"}`, then `{"confirm":fingerprint}` |
| POST | `/api/fanouts/{id}/rebase` | `{"candidate","dry_run":true}` -> plan, then `{"confirm":fingerprint}` |
| POST | `/api/fanouts/{id}/cleanup` | `{"candidates"?,"remove_worktrees"?,"force"?,"dry_run"?}` -> `{"items":[{"candidate","path","action","done","refusal","error"}],"fanout"}` |
| POST | `/api/fanouts/{id}/promote` | `{"task_id"?,"phase"?,"goal"?,"reference"?,"dry_run":true}` -> `{"plan":{"path","contents","task_id","phase"},"fingerprint","warnings"}`, then `{"confirm":fingerprint}` -> `{"written":{"path","task_id","queue"}}`; 404 not a flywheel repo, 409 exists / changed |
| GET | `/api/flywheel` | `{"enabled","folders":[summary]}`; summary `{"folder","generated_at","phases","counts":{state:n},"alerts":[{"kind","subject","detail","run_id"}],"live_runs","worker_live","autopilot_live","problem","worktree_of"}` |
| GET | `/api/flywheel/panel?folder=` | the whole panel (`phases`, `live`, `runs`, `worker`, `autopilot`, `claims`, `alerts`, `source`, `store`, `worktree_of`, `problem`); 404 unless an adopted workspace folder |
| GET | `/api/flywheel/run?folder=&run=&since=N` | `{"run_id","task_id","status","active","summary","from","entries":[{"ts","kind","header","body","error"}],"next"}`; poll with `since=next`. 400 bad run id, 404 unknown run, 503 while an earlier read still waits on the store |
| GET | `/api/flywheel/task?folder=&task=` | the task as a fan-out seed `{"folder","task_id","phase","goal","prompt","digest","graders","held_out":[name],"not_run","protect","protect_skipped"}` |
| POST | `/api/flywheel/worker` | `{"folder","workspace_id"?,"another"?,"cols"?,"rows"?}` -> 201 the new `fw worker` session; 409 `{"error","running":{"id","name"}}` while a hub-started worker runs there and `another` is not true |

Fan-out view: `{"id","name","base_sha","graders","winner","counts","leader","settled","candidates":[{"label","session_id","kind","model","branch","verdict","rank","why","grade","stale","cost_usd","pickable"}]}` (lists: no grader output). 503 fan-outs off.

## Shares and recordings

| Method | Path | |
|---|---|---|
| GET / POST / DELETE | `/api/shares[?session=]` | list `{"shares":[{"id","name","expires","viewers"}],"policy"}` / mint `{"session_id","expires_in"?,"name"?}` -> 201 `{"share","credential","path"}` (shown once) / revoke all (of `session`) |
| DELETE | `/api/shares/{which}` | id or label -> `{"revoked":[...]}`; open views close (4004) |
| POST | `/api/sessions/{id}/record/start` | `{"recording":{"name","session_id","title","part","size","started","modified","cols","rows","active"},"started"?}`: 201 started, 200 already; 409 not running |
| POST | `/api/sessions/{id}/record/stop` | as start; 409 not recording |
| GET | `/api/recordings` | `{"dir","limits":{"max_file_bytes","max_total_bytes"},"total_bytes","recordings":[recording]}` newest first, parts in order |
| GET / DELETE | `/api/recordings/{name}` | the `.cast` (`application/x-asciicast`, attachment) / 204 (409 while active) |

## Settings, config, plugins, notify, TLS

| Method | Path | |
|---|---|---|
| GET / PUT | `/api/settings` | GET `{"rev","values","keys","keymap","actions","rules"}`; PUT `{"rev","values"?:{id: value\|null},"keys"?:{action: [keys]\|null}}` -> view + `warnings`; 409 stale; 422 `{"errors":[{"action","key","message"}]}` |
| POST | `/api/settings/keys/check` | `{"keys":{action:[keys]}}` -> `{"keys","errors","warnings"}` (saves nothing) |
| GET | `/api/config` | `{"file":{"path","exists","error","loaded_at"},"rev","values":[{"key","value","source","apply"}],"pending_restart","running"}` |
| GET | `/api/plugins` | `{"enabled","dir","events","plugins":[{"name","dir","enabled","valid","error","description","version","timeout_secs","events","actions":[{"id","title","context"}],"last_run","last_error"}]}` |
| PUT | `/api/plugins/{name}` | `{"enabled"}` -> the plugin |
| GET | `/api/plugins/{name}/logs[?limit=N]` | `{"plugin","runs":[...]}` newest first (max 200) |
| POST | `/api/plugins/{name}/actions/{action}` | `{"session_id"?}` -> `{"ok","toast","paste","log"}`; 404 unknown, 409 disabled / plugins off / needs a session |
| GET | `/api/notify` | `{"push":null}` or `{"push":{"host","events","debounce_secs","settle_secs","detail"}}` |
| POST | `/api/notify/test` | 204 sent; 409 push off; 502 `{"error"}` refused / unreachable |
| GET | `/api/tls` | `{"mode","ca_fingerprint","constraints","names","outside","expires"}` |
| POST | `/api/tls/reload` | re-read the CA (`fw hub tls rotate` calls it) and re-issue now; closes TLS connections made under the old CA; 409 TLS off |

## Auth, machines, peers

| Method | Path | |
|---|---|---|
| POST | `/api/login-codes` | `{"code","expires_in":300}`: a one-time code for `/login?code=` |
| POST | `/api/token/rotate` | 204 (as `fw hub token --rotate`) |
| GET | `/api/whoami` | `{"scope":"full"\|"peer","peer","version","host","hub_id"}` |
| GET / POST | `/api/peers` | `{"peers":[{"id","name","created","last_used"}]}` / `{"name"}` -> 201 `{"peer","credential"}` |
| DELETE | `/api/peers/{which}` | revoke by id or name |
| DELETE | `/api/peer` | a peer credential revokes itself |
| GET / POST | `/api/machines` | `{"host","machines":[view]}` / `{"name","url","credential"\|"token","peer_name"?,"fingerprint"?,"replace"?}` -> 201 view (200 on `replace`); view `{"name","url","fingerprint","peer","added","status":{"state","since","error","version","host","presented_fingerprint"}}` |
| DELETE | `/api/machines/{name}` | `{"machine","revoked","note"}` |
| POST | `/api/machines/{name}/test` | `{"ok","latency_ms","whoami"\|"error","machine"}` |
| any | `/m/{name}/{*rest}` | proxy to machine `name`'s `/api/...` ([hub-security: Machines and peers](security.md#machines-and-peers)) |
| GET | `/api/events[?client=ID]` | events WebSocket (below) |

## Pages (no `/api`)

| Method | Path | |
|---|---|---|
| GET | `/` | the app (signed in), else the sign-in form |
| GET / POST | `/login` | `?code=` / `?token=`, or the form; sets the login cookie, then replaces itself with `/` (keeping `#session=`) |
| POST | `/logout` | ends this login, closes its sockets (4002) |
| GET | `/healthz` | `ok` (no credential) |
| GET | `/assets/{*path}` | UI assets (fonts, icons, `login.js` public; the rest signed in) |
| GET | `/manifest.webmanifest` | PWA manifest (public) |
| GET | `/sw.js` | service worker (public) |
| GET | `/replay` | recording player (`/replay#<name>`; its data needs a sign-in) |
| GET | `/ca` | local-CA install page (public, read-only; 404 without `--tls auto`) |
| GET | `/ca/` | same |
| GET | `/ca/hub-ca.crt` | CA certificate, DER |
| GET | `/ca/hub-ca.pem` | CA certificate, PEM |
| GET | `/ca/hub-ca.mobileconfig` | iOS profile |
| GET | `/ca/probe` | https probe: does this device trust the CA |
| GET | `/ca/ca.js` | the page's script |
| GET | `/ca/ca.css` | the page's style |
| GET | `/share/{credential}` | share-link viewer page |
| GET | `/share/{credential}/info` | `{"share":{"name","expires","now"},"session":{"name","workspace","kind","state","code","signal","title","agent","live","cols","rows"}\|null}` |
| GET | `/share/{credential}/ws` | read-only terminal socket |
| GET | `/share/{credential}/assets/{*path}` | the viewer's own assets (`viewer.js`, `term-view.js`, `viewer.css`, xterm) |
| any | `/share/{credential}/{*rest}` | 403 |

The pages answer an unreadable query string (and `POST /login` an unreadable or non-form body) with a 4xx page, or `{"error"}` to `Accept: application/json`.

## Terminal WebSocket

`/api/sessions/{id}/ws`. Server sends text `{"type":"snapshot","cols","rows","alt_screen","bytes"}`, then binary frames (the first `bytes` bytes are the snapshot, then live output).

| Server text message | |
|---|---|
| `{"type":"exit","code","signal"}` | process ended |
| `{"type":"reset"}` | fell behind: clear; a screen-only snapshot follows |
| `{"type":"size","cols","rows"}` | another viewer resized it (in order with output) |
| `{"type":"error","message"}` | input refused or dropped |

Clients size their terminal to `cols`x`rows`, replay the snapshot, then fit and send `resize`.

Client sends `{"type":"input","data":"..."}`, `{"type":"resize","cols":N,"rows":N}`, `{"type":"theme","foreground","background","cursor":"#rrggbb","ansi":[16 x "#rrggbb"]}` (what colour queries answer with), or binary raw input.

Close codes: **4001** token rotated, **4002** logout / login expired, **4003** peer revoked, **4004** share link ended, **1012** live handoff (reconnect: the session carries on). Through a machine proxy: **4010** (its 4001-4003), **4011** machine removed, **1011** link lost.

## Events WebSocket

`/api/events[?client=ID]` (`client`: 1-64 of `[A-Za-z0-9_-]`, else 400; omitted, the connection gets its own). Server sends:

| Message | When |
|---|---|
| `{"type":"snapshot","sessions":{id:status},"workspaces":{id:rollup}}` | on connect and after falling behind |
| `{"type":"status","session_id","status"\|null,"workspace_id","workspace_status"}` | per change, as this client sees it |
| `{"type":"sessions"}` | sessions or workspaces changed, or a terminal socket attached / left (`live.clients`): re-list |
| `{"type":"usage", ...}` | a session's usage changed |
| `{"type":"commands","session_id","fresh","commands":[...]}` | an agent suggested code blocks (newest first; `fresh`: written in the last 2 min) |
| `{"type":"flywheel","folder","summary"\|null}` / `{"type":"flywheel_reset"}` | a panel changed (`null`: no longer watched) / some missed: re-list `/api/flywheel` |
| `{"type":"layout","workspace_id","rev"}` | a layout saved or pruned |
| `{"type":"fanout","id"}` | a fan-out changed |
| `{"type":"settings","rev"}` / `{"type":"config","rev"}` | UI settings / hub.toml re-read |
| `{"type":"plugins"}` / `{"type":"plugin_toast","plugin","message","tone"}` | plugin list changed / a plugin's toast |

`sessions` is paced to one per 250 ms per client (never dropped); queued repeats fold. Client sends `{"type":"view","session_id":id|null,"visible"?:[ids]}`: the sessions on screen, which count as viewed (nothing while hidden).

## Plugins

Manifest: [hub-reference: plugin.toml](reference.md#plugintoml). A run:

- **Process**: cwd = the plugin dir, own process group, stdin = one JSON document `{"version":1,"type":"event"|"action","plugin","event"|"action","at","hub","payload"}`.
- **Environment**: a fixed allowlist (`PATH HOME USER SHELL LANG TZ TMPDIR TERM XDG_* DISPLAY ...`) plus `FW_HUB_PLUGIN`, `FW_HUB_PLUGIN_DIR`, `FW_HUB_PLUGIN_TRIGGER` (`event:<name>` / `action:<id>`), `FW_HUB_STATE_DIR`. Never the token or other secrets (it can still read the token file: it is you).
- **Limits**: SIGTERM at `timeout_secs`, SIGKILL 0.5 s later, to the group. 64 KiB of stdout/stderr kept. `plugins.max_concurrent` at once; past 64 waiting, events are dropped (logged).
- **Response** (stdout JSON object): `{"toast": "text" | {"message","tone":"info|ok|warn|error"}}`; actions may also return `{"paste":{"text","run"?,"session"?}}` (64 KiB max). Other stdout is just logged.
- **Logs**: `<state-dir>/plugin-logs/<name>.jsonl` (newest 200 runs past 512 KiB); failures also go to the hub log.

### Plugin events

`payload.session` (when there is one): `id`, `name`, `kind`, `state`, `workspace`, `workspace_id`, `cwd`, `worktree`, `branch`, `agent`.

| Event | Payload adds |
|---|---|
| `hub.started` | - |
| `session.created` | - |
| `session.exited` | `exit` |
| `session.removed` | - |
| `status.changed` | `status`, `previous`, `detail` |
| `review.committed` | `dir`, `head`, `branch`, `base`, `url`, `summary` (all `review.*`) |
| `review.pushed` | as above |
| `review.pr_opened` | as above |
| `review.rebased` | as above |
| `review.merged` | as above |
| `review.requested` | `{"author","reviewer","reused","kind","base","branch","dir"}` |
| `handoff.sent` | `{"from","to","source","bytes","truncated"}` (on the target's hub; findings: `source` `review_findings`) |
| `fanout.finished` | `fanout` (below); no `session` |
| `queue.delivered` | `item` `{"id","text" (first 2000 chars),"bytes","when","added_at","delivered_at"}`, `pending` |
| `queue.stalled` | `reason`, `item` (the next, same shape, `delivered_at` null; or null), `pending` |
| `share.created` | `share` `{"id","session_id","name","created","expires"}` (unix seconds); never the credential, its digest, or a link |
| `share.revoked` | as above; one per link a revoke ends; expiry sends none |
| `record.started` | `recording` `{"name","session_id","title","part","size","started","modified","cols","rows","active"}` (no path); `session` `null` once removed |
| `record.stopped` | as above plus `reason`: `stopped` (by hand) or `ended` (process exited, or writing failed); a hub stopping or a handoff carrying it on sends none |

`fanout.finished` fires once each time a grading leaves a fan-out settled (no agent working, nothing queued):

```json
{"fanout":{"id","name","workspace","base_sha","leader","winner","counts":{"candidates","passing","failing"},
  "candidates":[{"label","rank","verdict":"pass|fail|stale|pending|none","kind","model","branch",
    "graders_passed","graders_total","diff","tail","cost_usd","why","pickable"}]}}
```

Candidates come best first. `tail` = the last 400 characters of the first failing grader's stderr; no other grader output.

## Prompt queues

Behaviour: [hub-workflows: Prompt queue](workflows.md#prompt-queue). `{item}` = an id prefix (4+) or a position (1 = next). Every write returns the queue.

| Method | Path | |
|---|---|---|
| GET | `/api/queues` | every session's queue |
| GET / POST | `/api/sessions/{id}/queue` | GET `{"session_id","session","paused","stalled"?:{"at","reason","item"?},"last_delivered_at"?,"pending","items","waiting"?,"typing_quiet_secs"}` (400 a shell); POST `{"text","when"?}` -> 201 `{"item","queue"}` |
| PATCH / DELETE | `/api/sessions/{id}/queue/items/{item}` | PATCH `{"text"?,"when"?}`; 409 one being sent |
| POST | `/api/sessions/{id}/queue/items/{item}/move` | `{"position"}` (1-based among the pending) |
| POST | `/api/sessions/{id}/queue/items/{item}/editing` | `{"editing"}`: an editor has it open (held back; renew within 30 s) or closed it -> 204 |
| POST | `/api/sessions/{id}/queue/pause` | |
| POST | `/api/sessions/{id}/queue/resume` | also clears a stall |
| POST | `/api/sessions/{id}/queue/clear` | `{"all"?}`: the delivered history too |

- Item: `{"id","text","when","added_at","state":"pending|sending|delivered","sending_at"?,"delivered_at"?,"reconciled"?,"blocked_at"?,"position"?}`. `waiting`: why the next one is not sent now.
- Session listings carry `queue: {"pending","paused"?,"stalled"?,"next_when"?,"last_delivered_at"?}`.

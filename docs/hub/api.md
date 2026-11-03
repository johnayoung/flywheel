# Hub API

REST, WebSockets, events, and plugin payloads. `crates/flywheel-hub/src/docs_drift.rs` checks every route the server mounts is listed here. Front door: [../hub.md](../hub.md).

## Conventions

- JSON under `/api`; every route needs a credential: `Authorization: Bearer <token>`, a login cookie, or a peer credential (scripts use the bearer; one-time codes are not bearer credentials).
- Errors are `{"error":"..."}` (unknown endpoints 404, wrong methods 405 too); a malformed body is 400, a JSON body of the wrong shape 422, an unreadable query string 400 in the caller's words (`limit must be a number`).
- Cross-origin `Origin` headers are refused on WebSocket upgrades and on every non-GET request.
- `cols`/`rows` of 0 mean the default (120x32).
- Peer credentials get 403 on the routes listed under [security](security.md#machines-and-peers).

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
| GET / PUT | `/api/workspaces/{id}/layout` | `{"workspace_id","rev","tabs":[{"id","name","root"}]}`; node `{"type":"pane","id","session"\|null}` or `{"type":"split","dir":"right\|down","ratio","a","b"}`. PUT `{"rev","tabs"}` against `rev` (0: never saved) -> saved layout, `rev`+1; 409 `{"error","layout"}` stale; 400 bad ids (`[A-Za-z0-9_-]`, 1-32, unique), names (1-32 chars, unique per workspace), ratio outside 0.05..0.95, another workspace's session, a session twice in one tab, over 12 tabs / 12 panes |
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
| GET / POST | `/api/sessions[?workspace_id=]` | list / create `{"workspace_id","folder"?,"name"?,"kind"?,"command"?:[argv],"cols"?,"rows"?,"worktree"?:{"task","base"?},"prompt"?,"model"?,"permission_mode"?,"approval"?,"sandbox"?}` (`repo` = `folder`; `kind` default `shell`; `command` shells only, default `$SHELL`; default folder the first; a missing CLI or directory is a 400) |
| GET / PATCH / DELETE | `/api/sessions/{id}` | PATCH `{"name"}`; DELETE kills and forgets (204); `?remove_worktree=true[&force=true]` also removes companions + worktree: 200 `{"removed":[ids],"worktree":{"path","branch","branch_deleted"}}`, 409 with why |
| POST | `/api/sessions/{id}/kill` | 202: SIGHUP the session (SIGKILL after 3 s); record kept. 409 nothing left to kill |
| POST | `/api/sessions/{id}/relaunch` | an ended (`exited`/`stopped`) session in place: same id, name, command, folder; fresh PTY. Body `{"cols"?,"rows"?}`. 409 while it runs |
| POST | `/api/sessions/{id}/restart` | old name of `/relaunch` |
| GET | `/api/sessions/{id}/screen[?source=visible\|recent&lines=N]` | rendered text (below) |
| GET | `/api/sessions/{id}/explain` | `{"session","kind","status"\|null,"source":{"kind":"hook\|manifest\|heuristic\|hub\|none",...},"source_label","screen":{"state","rule","detail","evidence":[{"row","text","matched"}],"fallback"}\|null,"manifest":{"file","version","origin","path"?,"rules":[{"id","state","matched","reason"}]},"problems":[{"path","error"}],"note"}` |
| POST | `/api/sessions/{id}/input` | `{"data","paste"?,"enter"?}` -> `{"bytes","bracketed"}` (`paste`: as `fw hub send`). 409 no running terminal / not reading |
| POST | `/api/sessions/{id}/paste` | `{"text","run"?}` -> `{"bracketed","ran","bytes"}`: as `/input` with `paste`, plus trailing newlines dropped, at most 1 MiB, whitespace-only 400; 409 not running, not reading, or unsafe multi-line |
| GET | `/api/sessions/{id}/commands[?limit=N&all=true]` | suggested blocks newest first (shell only unless `all`; `limit` 1..500, default 50): `{"session_id","conversation","transcript","commands":[{"seq","conversation","message_id","timestamp","block_index","lang","shell","text","command","source"}]}` |
| GET | `/api/sessions/{id}/transcript[?around=N&before=N&after=N]` | `{"session_id","conversation","transcript","first","next","complete","messages":[{"index","message_id","role","kind","tool"?,"timestamp","truncated"?,"text"}],"error"?}`: `around-before..=around+after` (default 30 each, at most 200; without `around` the newest `before`); 400 for a shell |
| POST | `/api/sessions/{id}/companion` | `{"cols"?,"rows"?,"text"?,"run"?}`: the agent's companion shell (201 started, 200 running); with `text` also pastes once ready: `{"session","created","ready":"running\|bracketed\|quiet\|timeout","paste"}` |
| GET | `/api/sessions/{id}/worktree` | `{"worktree","path","branch","base","missing","dirty","untracked","unmerged","refusal","companions","blocking"}` (runs git) |
| GET | `/api/worktrees/plan?folder=&task=&base=` | `{"dir","default_base","branches","plan":{"path","branch","base"}\|null,"error"}` |
| GET | `/api/sessions/{id}/ws` | terminal WebSocket (below) |
| GET | `/api/kinds` | `{"kinds":[{"kind","available","program"}]}` |
| GET | `/api/usage` | hub and per-workspace `today` / `week` / `total` |
| GET | `/api/search?q=[&regex=&case=&ws=&session=&kind=&source=&limit=&per_session=&offset=]` | below |

Session views carry `agent` (`{"status","detail"?,"since"}`, agents only), `agent_session_id`, `launch` (with `unchecked: true` under `bypassPermissions`), `usage`, `worktree`, `companion_of`, `reviewer` / `review_of`, `recording` (while recording), `live.clients`, `live.link_viewers` (when any), and `git` (`{"branch","dirty","untracked","ahead","behind","against"?}`; absent until read or outside git).

**Screen text**: `source=visible` (default) is the screen's rows (`lines` refused); `recent` the last `lines` (1..100000, default 100) logical lines of scrollback + screen, soft wraps joined, trailing blank rows dropped. Both read the active screen.

```json
{"source":"visible","cols":80,"rows":24,"alt_screen":true,"title":"vim","cursor":{"row":2,"col":2,"visible":true},
 "modes":["app_cursor","bracketed_paste","mouse_drag","sgr_mouse"],"lines":["..."]}
```

**Search**: `{"query","regex","case_sensitive","sources","groups":[{"session_id","session","workspace_id","workspace","kind","state","terminal","conversation","counts":{"scrollback","transcript","commands"},"more","hits":[hit]}],"hits","truncated","offset","partial","elapsed_ms","searched":{"sessions","lines","messages","commands"},"notes"}`; `kind`/`source` comma lists. Every hit: `source`, `matches`, `before`, `match`, `after`, `line`; scrollback adds `screen` (`primary|alternate`), `index`, `from_end`, `match_text`, `occurrence`; transcript `index`, `message_id`, `role`, `kind` (`text|tool`), `tool`, `timestamp`; commands `seq`, `lang`, `shell`, `timestamp`. 400 bad query/regex/params, 404 unknown `ws`/`session`.

## Review, handoff, peer review

| Method | Path | |
|---|---|---|
| GET | `/api/sessions/{id}/review[?base=]` | `{"session_id","session","agent","mergeable","dir","worktree","branch","head","base":{"name","source","sha","merge_base","is_branch"},"ahead","behind","upstream","remotes","commits","committed","staged","unstaged","untracked","conflicts","operation","rebasing"?}`; a set is `{"files":[{"path","old_path"?,"status","insertions","deletions","binary"}],"insertions","deletions","truncated"}` |
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
| POST | `/api/sessions/{id}/handoff/compose` | `{"source":{"kind":"selection","text","from"?:"terminal\|transcript"}\|{"kind":"transcript","index"}\|{"kind":"command","seq"}\|{"kind":"last_command"}\|{"kind":"diff","path"?,"old_path"?,"base"?},"max_bytes"?:256..98304,"cross_machine"?}` -> `{"from":{"session_id","session","kind","branch"?,"machine"?},"source","what","prompt","content_bytes","kept_bytes","truncated"}` (sends nothing) |
| POST | `/api/sessions/{id}/handoff` | `{"prompt","from"?,"source"?,"truncated"?}` -> `{"to","paste"}`: paste into agent `id`, then Enter (400 a shell / the source itself; at most 128 KiB) |

Review actions (commit, push, pr, rebase, abort, merge): `{"dry_run":true,...}` -> plan `{"action","summary","details","warnings","commands":[{"dir","program","args","display"}],"fingerprint","refusal","needs_rebase","defaults"?}`; `{"confirm":fingerprint,...}` runs -> `{"ok","summary","ran":[{"display","code","output"}],"conflicts","url"?,"head","worktree"?}`; 409 refused / stale / failed.

## Fan-outs and Flywheel

| Method | Path | |
|---|---|---|
| GET / POST | `/api/fanouts` | `{"enabled","runner","fanouts":[summary]}` newest first / launch `{"workspace_id","folder"?,"base"?,"name"?,"prompt","candidates":[{"kind","model"?,"permission_mode"?,"approval"?,"sandbox"?}],"graders":[CMD\|{"run","name"?}],"timeout_secs"?,"protect"?,"commit_instruction"?,"from_task"?}` -> 201 view |
| GET / DELETE | `/api/fanouts/{id}` | the ranked board / forget it (204; sessions and worktrees stay) |
| POST | `/api/fanouts/{id}/grade` | `{"candidate"}` -> 202 |
| POST | `/api/fanouts/{id}/fix` | `{"candidate","dry_run"?}` -> `{"prompt","paste"?}` |
| POST | `/api/fanouts/{id}/pick` | `{"candidate","dry_run":true}` -> `{"refusal","needs_rebase","merge":plan,"losers"}`, then `{"confirm":fingerprint}` |
| POST | `/api/fanouts/{id}/rebase` | `{"candidate","dry_run":true}` -> plan, then `{"confirm":fingerprint}` |
| POST | `/api/fanouts/{id}/cleanup` | `{"candidates"?,"remove_worktrees"?,"force"?,"dry_run"?}` -> `{"items":[{"candidate","path","action","done","refusal","error"}],"fanout"}` |
| POST | `/api/fanouts/{id}/promote` | `{"task_id"?,"phase"?,"goal"?,"reference"?,"dry_run":true}` -> `{"plan":{"path","relative","contents","task_id","phase","new_phase","phases","notes"},"fingerprint","reference","warnings"}`, then `{"confirm":fingerprint}` -> `{"written":{"path","relative","task_id","phase","listed","queue"},"plan","warnings","fanout"}`. 400 invalid (a task-seeded fan-out without `task_id` included), 404 not a flywheel repo / panel off, 409 changed or exists |
| GET | `/api/flywheel` | `{"enabled","folders":[summary]}`; summary `{"folder","generated_at","phases","counts":{state:n},"alerts":[{"kind","subject","detail","run_id"}],"live_runs","worker_live","autopilot_live","problem","worktree_of"}` |
| GET | `/api/flywheel/panel?folder=` | the whole panel (`phases`, `live`, `runs`, `worker`, `autopilot`, `claims`, `alerts`, `source`, `store`, `worktree_of`, `problem`); 404 unless an adopted workspace folder |
| GET | `/api/flywheel/run?folder=&run=&since=N` | `{"run_id","task_id","status","active","summary","from","entries":[{"ts","kind","header","body","error"}],"next"}`; poll with `since=next`. 400 bad run id, 404 unknown run, 503 while an earlier read still waits on the store |
| GET | `/api/flywheel/task?folder=&task=` | the task as a fan-out seed `{"folder","task_id","phase","goal","prompt","digest","graders","held_out":[name],"not_run","protect","protect_skipped"}` |
| POST | `/api/flywheel/worker` | `{"folder","workspace_id"?,"another"?,"cols"?,"rows"?}` -> 201 the new `fw worker` session; 409 `{"error","running":{"id","name"}}` while a hub-started worker runs there and `another` is not true |

Fan-out view: `{"id","name","workspace_id","workspace","folder","repo" (deprecated mirror),"base","base_sha","graders","sent_prompt","winner","counts","warnings","leader","settled","graded_at","candidates":[{"label","session_id","kind","model","branch","path","phase","verdict","rank","why","grade","stale","cost_usd","tokens","pickable",...}]}` (`"summary":true` in lists: no grader output, worktree reads up to 30 s old). 409 refused, 503 fan-outs off.

## Shares and recordings

| Method | Path | |
|---|---|---|
| GET / POST / DELETE | `/api/shares[?session=]` | list `{"shares":[{"id","session_id","session","name","created","expires","viewers","last_used"}],"policy":{"default_expires_in","max_expires_in"}}` / mint `{"session_id","expires_in"?\|"expires"?,"name"?}` -> 201 `{"share","session","credential","path"}` (shown once) / revoke all (of `session`) `{"revoked":[...]}` |
| DELETE | `/api/shares/{which}` | id or label -> `{"revoked":[...]}`; open views close (4004) |
| POST | `/api/sessions/{id}/record/start` | `{"recording":{"name","session_id","title","part","size","started","modified","cols","rows","active"},"started"?}`: 201 started, 200 already; 409 not running |
| POST | `/api/sessions/{id}/record/stop` | as start; 409 not recording |
| GET | `/api/recordings` | `{"dir","limits":{"max_file_bytes","max_total_bytes"},"total_bytes","recordings":[recording]}` newest first, parts in order |
| GET / DELETE | `/api/recordings/{name}` | the `.cast` (`application/x-asciicast`, attachment) / 204 (409 while active) |

## Settings, config, plugins, notify, TLS

| Method | Path | |
|---|---|---|
| GET / PUT | `/api/settings` | GET `{"rev","values","keys","keymap","prefs":[spec],"actions":[{"id","group","label","scope","indexed","defaults"}],"rules":{"reserved","terminal","shadowed","desktop"},"load_error","file"}`; PUT `{"rev","values"?:{id: value\|null},"keys"?:{action: [keys]\|null}}` -> the view + `warnings`; 409 stale `{"error","settings"}`; 422 `{"error","errors":[{"action","key","message"}]}` |
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
| any | `/m/{name}/{*rest}` | proxy to machine `name`'s `/api/...` ([security](security.md#machines-and-peers)) |
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

`/api/sessions/{id}/ws`. Server sends **text** `{"type":"snapshot","cols","rows","alt_screen","bytes"}`, then **binary** frames (the first `bytes` bytes are the snapshot, then live output), and **text** `{"type":"exit","code","signal"}` / `{"type":"reset"}` (fell behind: clear, a screen-only snapshot follows) / `{"type":"size","cols","rows"}` (a viewer resized it; in order with the output) / `{"type":"error","message"}`. Clients reset, size their terminal to `cols`x`rows`, replay the snapshot, then fit and send `resize` (the snapshot starts with RIS anyway).

Client sends `{"type":"input","data":"..."}`, `{"type":"resize","cols":N,"rows":N}`, `{"type":"theme","foreground","background","cursor":"#rrggbb","ansi":[16 x "#rrggbb"]}` (what colour queries answer with), or binary raw input.

Close codes: **4001** token rotated (also when the upgrade raced the rotation), **4002** logout / login expired or evicted, **4003** peer revoked, **4004** share link revoked or expired, **1012** live handoff (reconnect: the session carries on; an `input` refused mid-handoff answers `{"type":"error"}`), and through a machine proxy **4010** (machine's 4001-4003), **4011** machine removed, **1011** link lost.

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

`sessions` is paced per client (at most one per 250 ms; a sooner one is sent when the interval is up, never dropped); repeats queued together fold (`status` / `usage` per session, `fanout` per id). Client sends `{"type":"view","session_id":id|null,"visible"?:[ids]}` (the focused pane's session and the other panes on screen; all count as viewed; nothing while hidden). Token rotation closes it with 4001.

## Plugins

Manifest: [reference](reference.md#plugintoml). A run:

- **Process**: cwd = the plugin dir, own process group. stdin = one JSON document `{"version":1,"type":"event"|"action","plugin","event"|"action","at","hub":{"version"},"payload"}`. Environment: only `PATH HOME USER LOGNAME SHELL LANG LANGUAGE LC_ALL LC_CTYPE LC_MESSAGES TZ TMPDIR TERM XDG_* DISPLAY WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS` from the hub, plus `FW_HUB_PLUGIN`, `FW_HUB_PLUGIN_DIR`, `FW_HUB_PLUGIN_TRIGGER` (`event:<name>` / `action:<id>`), `FW_HUB_STATE_DIR` -- never `FW_HUB_TOKEN`, peer/report secrets, or DSNs (it can still read the token file: it is you).
- **Limits**: at the deadline SIGTERM, then SIGKILL 0.5 s later, to the whole group; a run also ends 0.5 s after its command does even if something it started left the group and holds its output open (not killed; the run's error says so). An action keeps running, and is logged, if its HTTP caller hangs up. stdout/stderr: 64 KiB each kept (a JSON response over that is an error). At most `plugins.max_concurrent` runs at once; 64 waiting, then events are dropped (logged).
- **Response**: stdout that is a JSON object: `{"toast": "text" | {"message","tone":"info|ok|warn|error"}}` (events: every open UI; actions: the caller), and for actions only `{"paste":{"text","run"?,"session"?}}` (into the action's session unless `session`; as `POST /paste`, 64 KiB max). Other stdout is just logged; a malformed object is the run's error.
- **Logs**: every run to `<state-dir>/plugin-logs/<name>.jsonl` (0600; trigger, exit / signal, `timed_out`, duration, error, 4 KiB of each stream, response; trimmed to the newest 200 past 512 KiB); failures also to the hub log.

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
| `share.created` | `share` `{"id","session_id","name","created","expires"}` (unix seconds); never the credential, its digest, or a link |
| `share.revoked` | as above; one per link a revoke ends; expiry sends none |
| `record.started` | `recording` `{"name","session_id","title","part","size","started","modified","cols","rows","active"}` (no path); `session` `null` once removed |
| `record.stopped` | as above plus `reason`: `stopped` (by hand) or `ended` (process exited, or writing failed); a hub stopping or a handoff carrying it on sends none |

`fanout.finished`: a grading left a fan-out settled (no agent at work, nothing queued or grading); once per settling. `payload.fanout`: `{"id","name","workspace_id","workspace","folder","repo" (deprecated mirror),"base","base_sha","graded_at","leader","winner","counts":{"candidates","passing","failing","grading","running"},"warnings","candidates":[...]}`, candidates best first: `{"label","rank","verdict":"pass|fail|stale|pending|none","passed","kind","model","session_id","session","branch","path","head","graders_passed","graders_total","diff":{"files","insertions","deletions","untracked"},"failure","tail","cost_usd","tokens","why","pickable"}`. No grader output but `tail`: the last 400 characters of the first failing grader's stderr (stdout if empty). A subscription with `kinds` never matches it.

<!-- Prompt queue (hub/35): add its routes and events here. -->

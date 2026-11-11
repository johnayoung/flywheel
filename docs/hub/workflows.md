# Hub workflows

Front door: [hub](../hub.md).

## Scripting agents

```bash
id=$(fw hub new demo --kind claude --name fixer)
fw hub wait "$id" --until idle --timeout 60  # agent up, waiting for a prompt
fw hub send "$id" $'Fix the failing test in src/lib.rs\n'
fw hub wait "$id" --until done --changed --timeout 900 || { fw hub read "$id"; exit 1; }
fw hub commands "$id" --last > next.sh  # newest shell block it suggested
fw hub new demo --name build -- sh -c "$(cat next.sh)" && fw hub wait build --until exit  # or send it to a shell
fw hub wait "$id" --until blocked --timeout 5 && fw hub read "$id"  # 0: it waits on a prompt
```

`wait`: 0 met, 124 timeout, 1 error (or session ended). `--changed`: only statuses begun after it started. `--until idle` accepts `done`; `exit` prints `exited N` / `killed (SIG)`; `--timeout 0`: check once.

## Agent launcher

- **Resume** (`--resume ID`, `--continue`, the dialog's **Conversation**): a new session resumes a folder's Claude Code or Codex conversation (`fw hub conversations WS --kind K`), unless a running one has it open. Relaunch and restarts resume a session's own.
- **Accounts** (Claude): `claude setup-token` as the other account, Settings > Agents (or `fw hub account add work`), then `--account work` or the dialog's **Account** (per workspace). Spawns get `CLAUDE_CODE_OAUTH_TOKEN` (one `~/.claude` serves both; rows show `@work`); Claude's settings `env` outranks it.
- **First prompt** (`--prompt`, 32 KiB): first user turn, create only. **Model**: `claude --model` / `codex -m`.
- **Permissions**: Claude `--permission-mode` (`bypassPermissions` warns, marks it `unchecked`); Codex `-a` approval, `-s` sandbox; reapplied on relaunch and restore.
- **Templates** (per workspace): kind, prompt, model, permissions, worktree; a taken name needs `--replace` (all) or `--edit` (given flags). Launch: palette, `new --template NAME` (flags win). Defaults: `[agents.*]`.

## Suggested commands

Fenced blocks from the agent's own record, **exactly as written**.

- Sources: Claude's transcript (or the Stop hook's `last_assistant_message`, so blocks are in before `done`), Codex's rollout, else its `notify` payload. Tailed per second and per hook report; newest 500 per session kept.
- Shell blocks: `bash sh zsh shell console fish ...`, or untagged ones starting like a command; `console` keeps only commands.
- In-session commands (Claude Code's `!` bash mode): an inline `` `! cmd` `` or a block of `! cmd` lines is a shell command; Copy / Send drop the `!`.
- Send pastes (bracketed if enabled), Send + Run adds Enter; multi-line without bracketed paste: 409, use Copy. `--copy`: OSC 52, over SSH too (tmux: `set -g set-clipboard on`).
- **Commands drawer** (`Alt C`): newest first; Send targets the companion shell (opened as needed). Keys: `j`/`k`, `c` copy, `1`-`9` a card, `s` send, `Shift S` send + run.
- **Replies**: every assistant message as written (newest 100, 64 KiB each). `Alt Y` copies the last; the Replies tab copies one whole or by block. A reply in an agent's pane (not a tool call) has a hover copy.

## Usage and cost

- From transcripts (Claude `message.usage` incl. subagents, Codex `token_count`) at list prices (`usage.rs` `PRICES`; unknown models: tokens only); ~1-2% under Claude's (side calls untranscribed).
- Header meter, sidebar, `fw hub ls`; 62 days per session.

## Worktrees and companion shells

```bash
fw hub new WS --kind claude --worktree fix-retry [--base main]  # git worktree add -b <prefix>fix-retry ../<repo>.worktrees/fix-retry
fw hub kill fix-retry --rm --remove-worktree  # refused if work would be lost, unless --force
```

- A sibling of the **main** checkout; base `--base`, else the current branch; bad names, existing dirs or branches refused. Removal: 409 while another session uses it, or (without `--force`) over uncommitted, untracked, or unmerged work; the branch goes only if nothing is lost.
- **Companion shell**: one per agent, in its dir (`Alt \``, the drawer, the API), closed with it; the `shell`/`agent` pill flips.

## Review

Any session in a git folder: pill `review`, `Alt G`, `fw hub review`. Base: the worktree's, else upstream (`--base`). **Every action is planned first**, then run by its `fingerprint` ([hub-api](api.md#review-handoff-peer-review)).

| Action | Runs | Refused when |
|---|---|---|
| Commit all | `git add -A` (but ticked files); `git commit -F -` (hooks run) | what `add` staged differs from the plan |
| Push | `git push <remote> <planned HEAD>:refs/heads/<B>` | no remote; never forced |
| Open PR | `gh pr create --base --head B --body-file -` | not pushed / out of sync, no `gh` auth |
| Rebase onto base | `git rebase <base>` in the worktree | uncommitted changes (conflicts stop; Abort) |
| Merge into base | `git -C <main checkout> merge --ff-only <planned commit>` | main checkout dirty or off base, not a fast-forward |

- **Commit all** checks every changed path. Ticked: `generated` (`node_modules/`, `__pycache__/`, `.venv/`, `*.egg-info/`, `.pyc`, a dir with `CACHEDIR.TAG` or `pyvenv.cfg`, `target/` by `Cargo.toml`);
  `secret` (`.env*`, keys by name or content, credentials, `*.tfvars`), `personal` (`.claude/settings.local.json`, `CLAUDE.local.md`), `junk` (`.DS_Store`, `*.log`). Unticked: `build` (`dist/`, `build/`, `coverage/`, `venv/`, other `target/`), `large` (5 MiB+).
- No git identity: refused before staging; a failed step restores the index. **Merge** and fan-out **pick** refuse a branch adding a ticked kind unless `--include-suspects`.
- "Ask the agent" pastes a commit-message or conflict prompt. Runner: argv, no shell, deadlines, one action per repo, logged.

## Pull requests and CI

A branch with a PR: a badge on its row (`#7`, checks), a PR section in Review, `fw hub pr SESSION`. All via `gh` (missing or signed out: the section says why).

- **Asked** per (folder, branch) by head (`gh pr list --head B`; `123` never binds PR #123), if pushed, not default: checks running 1 min, open or none 5 min, merged/closed 30 min; at once after Push, Open PR, own actions; Refresh (5 s apart). At most 30 gh calls/min; a rate limit pauses 15 min (state kept, stale).
- **View log**: `gh run view --job J --log-failed`, last 400 lines, escapes stripped, secrets redacted (Activity's rules).
- **Ask agent to fix CI**: failing checks + the log's last 150 lines, pasted at rest, else queued (`done`). One ask per PR waits (newer replaces it), dropped once checks pass or the PR closes; not re-pasted within 5 min.
- **Re-run failed** (`gh run rerun R --failed`), **Ready** (`gh pr ready`),
  **Merge** (`gh pr merge N --<method> --match-head-commit SHA`, the repo's default method): planned, fingerprint-checked against a GitHub re-read (unreadable: refused). Refuses closed, draft, conflicting; checks failing or running, changes requested, review required: only with the number typed back (`--force N`).
- **Events** `pr.checks_failed`, `pr.checks_passed` (from running or red), `pr.review_changes_requested`, `pr.merged`; a hub start announces nothing.

## Handoff and peer review

- **Send to agent** (`fw hub handoff FROM TO`, `handoff.send`): a selection, message, block, or diff to another agent (any workspace or machine) as one editable prompt (32 KiB).
- **Shell output to agent** (`Alt O`, a shell's header **to <agent>**, send icon or pane menu, `fw hub handoff SHELL AGENT --output`): the last prompt command's output, else the last 200 lines; a companion shell: its own agent.
- **Ask for a review** (`fw hub ask-review`, Review drawer, `review.ask`): a read-only reviewer of the other kind (`review.reviewer`; Claude `plan`, Codex `-a never -s read-only`) in the author's worktree, prompted with `git diff <base>...HEAD`; one per author. Model: `--model` > the author's (same kind) > `agents.<kind>.model`.
- **Send findings to <author>** pastes the reviewer's latest reply back. Pairing survives restarts; closing the author closes its reviewers.

## Verified fan-out

One prompt to 2-6 agents, each in its own sibling worktree from **the same base commit**, graded by commands the **hub** runs.

```bash
id=$(fw hub fanout new demo --prompt "Fix the flaky retry test" \
  --agent claude:opus --agent claude:sonnet --agent codex:gpt-5.1-codex \
  --grader 'unit=cargo test -p retry' --grader ~/oracles/retry-check.sh --permission-mode acceptEdits)
fw hub fanout wait "$id" --timeout 3600; fw hub fanout show "$id" -v
fw hub fanout fix "$id" c3 --yes  # paste c3's failing grader output into its agent
fw hub fanout pick "$id" c1 --yes [--rebase]
fw hub fanout cleanup "$id" [--force] --yes
```

- **Agent claims are untrusted**: a finished turn only queues grading; graders' exit codes decide. Needs you: once per unseen grading.
- **Graders are held out of the prompt only** (agents run as you): `--protect` a tracked grader's inputs (a change fails) or keep it outside the repo ([held-out gate](../held-out-gate.md)); absolute-path scripts are pinned by hash.
- Grading: flywheel-core's runner (in order, `/bin/sh -c`, exit 0 passes, `--timeout` 900 s each, first failure stops); 2 at once. Rank: passing (smaller diff, then lower cost), failing, stale, grading, ungraded; a no-change pass cannot be picked.
- **Pick** needs a fresh pass on a clean tree at exactly the graded commit, then ff-merges it (plan + fingerprint); base moved: `--rebase` (grades again).
- **Cleanup** removes losers' worktrees and sessions (unique work: `--force`; `--stop-only` keeps worktrees); `rm` forgets. Max 50 fan-outs; cut-short gradings rerun.

## Fan-outs and flywheel tasks

A folder with a flywheel **directory** work source:

```bash
fw hub fanout promote "$id" --reference c1  # instead of pick; dry run: path and exact bytes
fw hub fanout promote "$id" --reference c1 --yes  # writes <project>/tasks/active/NN-<slug>/<slug>-<tag>.json
fw hub fanout new demo --from-task fix-retry --agent claude:opus --agent claude:sonnet
```

- **Promote** to land via `fw worker` (refused after a pick): goal (`--goal`, seed task's, else prompt's first line), prompt -> `context.notes`, graders -> `command`, `--protect` -> constraints, `--reference` (default winner, else leader); refused for a used id. Then **Run <id> with worker here**.
- **Fan out this task** (Flywheel panel): prompt = its goal, context; graders = its command plus **held-out** graders, added server-side (none/unreadable: refused). A prompt naming a held-out grader is refused; Ask to fix only says one failed. Not a sandbox.

## Flywheel panel

Repos flywheel knows (project id) get the verified loop, read-only (`--no-flywheel`: off); state: `$FLYWHEEL_HOME/projects/<id>/`. Untrusted `.flywheel.toml`: shows `fw trust`. **Postgres DSN:** `store`, else hub env.

- Phases, tasks, `plans` (+archived: verified/total, spend, 24h verified), live runs (last tool call), 12 newest runs + grader receipts, worker and autopilot liveness, claims, alerts (**Needs you** until **Mark seen**). Open: the chip's `verified/total` badge, `Alt F`, palette.
- **Start worker here**: `fw worker` in a split; a task's **run**: `fw worker --task <id>` (offers unmet prereqs); a second asks (409); `queued in <session>`: a scoped worker runs it. Outcome: `verified`, `not verified`, `refused`, `worker exit N`. **Output**: `fw show` summary, redacted live transcript; message, interrupt (confirmed).
- **Autopilot** (`fw hub autopilot`): **Start** (confirmed: lands unattended) = `fw autopilot` in a session; stop = Ctrl-C; pause/resume = its `--pause|--resume`.
- Shown (ledger): tier verdicts, newest deciding cycle (emitted, dropped + why), 20 newest decisions, parked gates. **Approve** / **Deny** = `fw approve` / `fw reject` pinned to the gate (one per gate; deny takes feedback), each an audit row.
- A linked worktree reads its own store; `github*` sources: unlisted; no SQLite store made. Per-folder watcher (stat 2s, re-read 15s). Seam: `FlywheelProvider` (`hub_flywheel.rs`).
- **Audit**: session start/end (exit, last reply), merge, review, PR, handoff, fan-out grades, approvals: `hub.*` rows in the folder's project store (`fw audit --project`), if any. Seam: `AuditSink` (`hub_audit.rs`).

## Prompt queue

Per agent session, prompts the hub types itself, in order ("when done, do X"): `fw hub queue add api --when idle "now the changelog"`.

| | |
|---|---|
| `--when` | `done` (default): at rest (turn finished, or awaiting a first prompt); `idle`: at rest and viewed; `blocked-cleared`: at the front, blocked, answered, finished |
| Never | `working`, `blocked`, `unknown`; within 1.5 s of the rest or `queue.typing_quiet` of typing; during a grading, handoff, review, or edit |
| Drafts | text left in the agent's input line holds it (keys answering a prompt don't count) |
| One at a time | the next waits for a rest begun after the last delivery (bracketed paste, Enter); 20 delivered kept |
| Exactly once | `sending` saved before the paste, `delivered` after; a crash between: the transcript decides (`reconciled`) or it is pending again |
| Stalls | the agent exited, the paste was refused, or no turn started in 120 s (`queue.stalled`), until resumed or relaunched |
| Order | drag, up/down, `queue move`; pending editable; 50 pending, 64 KiB each |

**Broadcast**: one prompt to many agents (Ctrl-click rows, a workspace's **Send to all agents**, palette, `fw hub broadcast api web-1 "rebase"`); a workspace: its non-exited agents. Each target: sent if its queue would deliver now, else queued (`done`), else refused (why). `--header` names the recipients.

## Activity

What happened, newest first: palette **Activity**, the header pill, `fw hub activity` (24 h: digest, then timeline; `--since last-seen`: since this CLI last looked).

- **Rows**: the plugin events (`status.changed` only to `blocked`), `turn.finished`, `commands.suggested`, `usage`, `plugin.ran`, `hub.stopped`. A row opens its object or attaches.
- **Kept** in `activity.jsonl` (rotated at 1 MiB or weekly, 30 days): kind, ids, names, short facts; never terminal output, prompts, commands, grader output, paths; token-shaped words: `[redacted]`.
- **While you were away** (back after 10+ min): turns done, blocked now, fan-outs graded, commands suggested, cost. **Machines**: asked at once (4 s each), merged, labelled; a silent one: `partial`.

## Fleet and work

Every agent session's turns, tool calls (duration, outcome), subagents, plan, waits on you, context window, cost, and rate limits are recorded (`telemetry.db`, 30 days) and folded into what it is doing now.

- **Fleet** (the live count, palette **Fleet board**, `fw hub fleet [SESSION]`): every session, urgent first: status, turn and tool, progress, context %, cost, links. Machines' sessions too.
- **Needs you** (sidebar, `fw hub inbox`): blocked, then `asking` (a closing question, or an `ask_operator` reply, pick, or command), answered in the row.
- **Handoffs**: an agent's `handoff` prompt waits on Needs you; **Start session** (`fw hub continue`) runs it in the same directory, linked. Agents coordinate with `list_sessions` and `message_session` (asks until allowed; typed in or queued under `From <session>`). `fw hub new` inside an agent links to it.
- **Captured runs** (`fw hub capture install`): `claude -p`, SDK, terminal runs roll up under their hub session or `FLYWHEEL_BATCH` (`fw worker` sets it) vs `FLYWHEEL_BATCH_BUDGET`.
- **Work** (the header's **work** pill): one session's plan, then its turns with their subagents and calls. A working session's sidebar row shows its plan's bar and step, else its tool.
- **Progress** is observed (turns, subagents, calls) and, when the agent keeps a plan, declared: its todo or task tools, Codex's plan, or the hub's `report_progress` tool (every Claude session gets the hub's tools and one system-prompt paragraph on them). A declared status is the agent's claim, shown beside its calls; no plan, no percentage.
- **Capture**: Claude Code's hooks POST to the hub (a process per event only for `SessionStart`, until a status line has shown a CLI of 2.1.322 or later, or under `allowedHttpHookUrls`); its status line carries the context window, cost, and the 5 h / 7 d gauges (your own status line still shows: the hub's runs it). Codex is read from its rollout.
- **Many hosts**: `[telemetry] upstream = "NAME"` sends this hub's events to that machine (`fw hub machine add`) in batches; while it is down they wait here. Its Fleet lists every hub's sessions.

## Digests

Off until `hub.toml` `[digest] times` is set ([hub-reference](reference.md#hubtoml); or Settings > Digest).

- **Says**, since the last digest, per workspace: turns done, agents blocked now, fan-outs graded (winner / leader), PRs red / green / merged, autopilot decisions and gates waiting, queue stalls, spend today and this week (local midnight, Monday); then the top 3 needs-you (gates, blocked, red PRs, stalled queues), linked.
- **To** `channels`: `push` (notify.toml; names and counts only, the 3 as ntfy buttons), `browser` (a card; with alerts on, a notification), `plugin` (`digest.ready`). Empty: skipped (`quiet_if_empty`). `delivery.channels`: only those that heard it.
- **Once per slot**: local times per date (a skipped 02:30 runs at 03:00, a repeated 01:30 once); `digest.json` under a lock across restarts and `upgrade`. A slot missed while down is sent on start within 2 h, else skipped; a new schedule owes nothing before it. A send claim over 5 min old is dropped.

## Permissions

Palette **Permissions: FOLDER**, `fw hub perms show`.

- **View**: Claude Code sources highest first (managed `/etc/claude-code` + `managed-settings.d`, the hub's `--settings` + `--permission-mode`, local, project, user) with each rule; Codex trust (`[projects."<repo>"]`), `approval_policy`, `sandbox_mode`, `rules/*.rules`, and their sources.
- **Always allow** (why?, quick answers' `always`, the sheet): the request from the `PermissionRequest` hook, else the dialog on screen, as the narrowest rule (`Bash(cargo test -p app *)`: program, subcommand, scoping flags; else the exact command; one per command of a line).
- Written to `.claude/settings.local.json` (warned when tracked; `--gitignore` ignores it) or, opt-in, user. Preview: file, diff, "This lets the agent ...", an overriding deny/ask. Atomic, keys and layout kept; Yes is typed only if the dialog on screen is that request; the rule is typed back to confirm.
- **Never proposed**: every command, `rm`, `sudo`, `curl`/`wget`, interpreters, `git push` (also `git -C dir push`), `git *`, `cp`/`mv`/`tee`/`sed`/`awk *`, `gh pr merge`, `fw`/`flywheel`, `find *`, exec subcommands (`npm exec`, `git rebase`), `timeout 5 *`, whole MCP servers, pipes into a shell,
  substitutions, `&&` `;` `|`, writes outside the repo or to `.claude/` `.codex/` `.git/` `.gitmodules` `.mcp.json`.
- **Suggestions**: `permission-requests.jsonl` (0600, 30 days; rules, never commands) per repo, most frequent first, minus covered ones.
- **Codex**: prefix rules to `.codex/rules/default.rules` (loaded once trusted) or `~/.codex`; `trust_level` in `$CODEX_HOME/config.toml`, `approval_policy` / `sandbox_mode` in `.codex/config.toml`.
  Typed back: `never`, `danger-full-access`, trusting a project whose `.codex/` holds those, a dangerous rule, or `notify`/`mcp_servers`/hooks; a symlinked `.codex` is refused. Writes log and emit `perms.allowed`.

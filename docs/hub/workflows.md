# Hub workflows

Driving agents from scripts and the UI. Front door: [hub](../hub.md).

## Scripting agents

```bash
id=$(fw hub new demo --kind claude --name fixer)
fw hub wait "$id" --until idle --timeout 60  # agent up, waiting for a prompt
fw hub send "$id" $'Fix the failing test in src/lib.rs\n'
fw hub wait "$id" --until done --changed --timeout 900 || { fw hub read "$id"; exit 1; }
fw hub commands "$id" --last > next.sh  # newest shell block it suggested, exactly
fw hub new demo --name build -- sh -c "$(cat next.sh)" && fw hub wait build --until exit  # or send it to a shell
fw hub wait "$id" --until blocked --timeout 5 && fw hub read "$id"  # 0: it waits on a prompt
```

`wait`: 0 met, 124 timeout, 1 error (or the session ended). `--changed`: only statuses begun after it started. `--until idle` accepts `done`; `exit` prints `exited N` / `killed (SIG)`; `--timeout 0` checks once.

## Agent launcher

- **Resume** (`--resume ID`, `--continue`, the dialog's **Conversation**): a new session picks up one of the folder's Claude Code or Codex conversations (`fw hub conversations WS --kind K`); one open in a running session is refused. Relaunch and hub restarts already resume a session's own conversation.
- **Accounts** (Claude): `claude setup-token` as the other account, Settings > Agents (or `fw hub account add work`), then `--account work` or the dialog's **Account** (per workspace). Spawns get `CLAUDE_CODE_OAUTH_TOKEN`: one `~/.claude` serves both; rows show `@work`. A key in Claude's settings `env` outranks it.
- **First prompt** (`--prompt`, 32 KiB): the first user turn, on create only. **Model**: `claude --model` / `codex -m`.
- **Permissions**: Claude `--permission-mode` (`bypassPermissions` warns, marks the session `unchecked`); Codex `-a` approval, `-s` sandbox; reapplied on relaunch and restore.
- **Templates** (per workspace): kind, prompt, model, permissions, worktree; a taken name needs `--replace` (all) or `--edit` (given flags). Launch: palette, `new --template NAME` (flags win). Defaults: `[agents.*]`.

## Suggested commands

Each fenced block is read from the agent's own record and served **exactly as written**.

- Sources: Claude's transcript (or the Stop hook's `last_assistant_message`, so blocks are in before `done`), Codex's rollout, else its `notify` payload. Tailed each second and per hook report; newest 500 per session persist.
- Shell blocks: `bash sh zsh shell console fish ...`, or untagged ones that start like a command; `console` keeps only the commands.
- In-session commands (Claude Code's `!` bash mode): an inline `` `! cmd` `` or a block of `! cmd` lines is a shell command; Copy / Send drop the `!`.
- Send pastes (bracketed when enabled), Send + Run adds Enter; multi-line without bracketed paste: 409, use Copy. `--copy`: OSC 52, over SSH too (tmux: `set -g set-clipboard on`).
- **Commands drawer** (`Alt C`): newest first; Send targets the companion shell (opened if needed). Keys: `j`/`k`, `c` copy, `1`-`9` a card, `s` send, `Shift S` send + run.
- **Replies**: every assistant message as written (newest 100, 64 KiB each). `Alt Y` copies the last; the Replies tab copies one whole or by block. Hovering a reply in an agent's pane (not a tool call) shows a copy button.

## Usage and cost

- From transcripts (Claude `message.usage` incl. subagents, Codex `token_count`) at list prices (`usage.rs` `PRICES`; unknown models: tokens only); ~1-2% under Claude's own (side calls are untranscribed).
- Header meter, sidebar, `fw hub ls`, `GET /api/usage`; 62 days per session.

## Worktrees and companion shells

```bash
fw hub new WS --kind claude --worktree fix-retry [--base main]  # git worktree add -b <prefix>fix-retry ../<repo>.worktrees/fix-retry
fw hub kill fix-retry --rm --remove-worktree  # refused if work would be lost, unless --force
```

- A sibling of the **main** checkout; base `--base`, else the current branch; bad names, existing dirs or branches refused. Removal: 409 while another session uses it, or (without `--force`) over uncommitted, untracked, or unmerged work; the branch goes only when nothing is lost.
- **Companion shell**: one per agent, in its directory (`Alt \``, the drawer, the API), closed with it; the `shell` / `agent` pill flips.

## Review

Any session in a git folder: pill `review`, `Alt G`, `fw hub review`. Base: the worktree's, else the upstream (`--base`). **Every action is planned first**, then run by its `fingerprint` ([hub-api](api.md#review-handoff-peer-review)).

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

A branch with a pull request: a badge on its row (`#7`, checks), a PR section in Review, `fw hub pr SESSION`. All through `gh` (missing or signed out: the section says why).

- **Asked** per (folder, branch) by head (`gh pr list --head B`; a branch named `123` never binds PR #123), if pushed and not a default branch: checks running 1 min, open 5 min, merged/closed 30 min, none 5 min; at once after Push, Open PR, its own actions; Refresh (5 s apart). At most 30 gh calls/min; a rate limit pauses 15 min (state kept, marked stale).
- **View log**: `gh run view --job J --log-failed`, the last 400 lines, escapes stripped, secrets redacted (Activity's rules, `NAME=value`, `password:`, `--password V`, AWS keys, URL passwords).
- **Ask agent to fix CI**: failing checks + the log's last 150 lines, pasted at rest, else queued (`done`). One ask per PR waits (newer replaces it), dropped once checks pass or the PR closes; never pasted twice in 5 min.
- **Re-run failed** (`gh run rerun R --failed`), **Ready** (`gh pr ready`),
  **Merge** (`gh pr merge N --<method> --match-head-commit SHA`, the repo's default method): planned, confirmed by fingerprint against GitHub read again (unreadable: refused). Merge refuses closed, draft, conflicting; checks failing or running, changes requested, review required: only with the number typed back (`--force N`).
- **Events** `pr.checks_failed`, `pr.checks_passed` (from running or red), `pr.review_changes_requested`, `pr.merged`; a hub start announces nothing.

## Handoff and peer review

- **Send to agent** (`fw hub handoff FROM TO`, `handoff.send`): a selection, message, block, or diff to another agent (any workspace or machine) as one editable prompt (32 KiB).
- **Shell output to agent** (`Alt O`, header **to <agent>** on a shell, the send icon or pane menu on a shell pane, `fw hub handoff SHELL AGENT --output`): the output of the last command typed at the shell's prompt (found by the prompt line it sits at now), else its last 200 lines; a companion shell defaults to its own agent.
- **Ask for a review** (`fw hub ask-review`, Review drawer, `review.ask`): a read-only reviewer of the other kind (`review.reviewer`; Claude `plan`, Codex `-a never -s read-only`) in the author's worktree, prompted with `git diff <base>...HEAD`; one per author. Model: `--model`, else the author's (same kind), else `agents.<kind>.model`.
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

- **Agent claims are untrusted**: a finished turn only queues grading; the verdict is the graders' exit codes. Needs you: once per unseen grading.
- **Graders are held out of the prompt only** (agents run as you): `--protect` a tracked grader's inputs (a change fails) or keep it outside the repo ([held-out gate](../held-out-gate.md)); absolute-path scripts are pinned by hash.
- Grading: flywheel-core's runner (in order, `/bin/sh -c`, exit 0 passes, `--timeout` 900 s each, first failure stops); 2 at once. Rank: passing (smaller diff, then lower cost), failing, stale, grading, ungraded; a pass changing nothing cannot be picked.
- **Pick** needs a fresh pass on a clean tree at exactly the graded commit, then ff-merges it (plan + fingerprint); base moved: `--rebase`, which grades again.
- **Cleanup** removes losers' worktrees and sessions (unique work: `--force`; `--stop-only` keeps worktrees); `rm` forgets. Max 50 fan-outs; cut-short gradings rerun.

## Fan-outs and flywheel tasks

For a folder flywheel drives with a **directory** work source:

```bash
fw hub fanout promote "$id" --reference c1  # instead of pick; dry run: path and exact bytes
fw hub fanout promote "$id" --reference c1 --yes  # writes .flywheel/tasks/active/NN-<slug>/<slug>-<tag>.json (uncommitted)
fw hub fanout new demo --from-task fix-retry --agent claude:opus --agent claude:sonnet
```

- **Promote instead of picking** to land via `fw worker` (refused after a pick): goal (`--goal`, the seed task's, or the prompt's first line), prompt -> `context.notes`, graders -> `command` graders, `--protect` -> constraints, `--reference` (default winner, else leader); refused for an existing file or id. Left **uncommitted**; **Run <id> with worker here** runs `fw worker --task <id>`.
- **Fan out this task** (Flywheel panel): prompt from its goal and context; graders = its command plus **held-out** graders, added server-side (none, or an unreadable registry: refused). A prompt naming a held-out grader is refused; Ask to fix only says one failed. Not a sandbox.

## Flywheel panel

Folders with a `flywheel.toml` or `.flywheel/` get the verified loop beside the terminals, read-only (`--no-flywheel`: off). **Postgres stores read the DSN from the hub's own environment** (`FLYWHEEL_PG_DSN`, else `DATABASE_URL`).

- Phases, tasks, live runs, the 12 newest runs with grader receipts, worker and autopilot liveness, claims, alerts (**Needs you** until **Mark seen**). Open: the folder chip's `verified/total` badge, `Alt F`, the palette.
- **Start worker here**: `fw worker` in a split below; a task's **run**: `fw worker --task <id>` (asks to add unmet prerequisites); a second asks first (409); `queued in <session>`: a scoped worker will run it. Outcome: `verified`, `not verified`, `refused`, `worker exit N`. **Output**: `fw show` summary and redacted transcript, live.
- **Autopilot** (`fw hub autopilot`): **Start** (confirmed: lands unattended) runs `fw autopilot` in a hub session; stop = Ctrl-C; pause / resume = `fw autopilot --pause|--resume`.
- Shown: tier verdicts, the newest deciding cycle (emitted, dropped with reasons), the 20 newest decisions, parked gates. **Approve** / **Deny** = `fw approve` / `fw reject` pinned to the gate shown (one per gate, then `decision pending`; deny retries with feedback). Peers: read-only.
- A linked worktree reads its own store; `github*` sources are not listed; a missing SQLite store is not made. One watcher per folder (stat 2 s, re-read 15 s). Seam: `flywheel::FlywheelProvider` (`hub_flywheel.rs`).

## Prompt queue

Per agent session, prompts the hub types in itself, in order ("when done, do X"): `fw hub queue add api --when idle "now the changelog"`.

| | |
|---|---|
| `--when` | `done` (default): at rest (turn finished, or awaiting a first prompt); `idle`: at rest and viewed; `blocked-cleared`: at the front, blocked, answered, finished |
| Never | `working`, `blocked`, `unknown`; within 1.5 s of the rest or `queue.typing_quiet` of typing; during a grading, handoff, review, or edit |
| Drafts | text left in the agent's input line holds it (keys answering a prompt do not count) |
| One at a time | the next waits for a rest begun after the last delivery (bracketed paste, Enter); 20 delivered kept |
| Exactly once | `sending` saved before the paste, `delivered` after; a crash between: the transcript decides (`reconciled`) or it is pending again |
| Stalls | the agent exited, the paste was refused, or no turn started in 120 s (`queue.stalled`), until resumed or relaunched |
| Order | drag, up/down, `queue move`; pending editable; 50 pending, 64 KiB each |

**Broadcast**: one prompt to many agents (Ctrl-click rows, a workspace's **Send to all agents**, palette, `fw hub broadcast api web-1 "rebase on main"`); a workspace means its non-exited agents. Each target: sent if its queue would deliver now, else queued (`done`), refused with why (shell, exited, full). `--header` names the recipients.

## Activity

What happened, newest first: palette **Activity**, the header pill, `fw hub activity` (24 h: the digest, then the timeline; `--since last-seen`: since this CLI last looked).

- **Rows**: the plugin events (`status.changed` only to `blocked`), `turn.finished`, `commands.suggested`, `usage`, `plugin.ran`, `hub.stopped`. A row opens its object, else attaches.
- **Kept** in `activity.jsonl` (rotated at 1 MiB or a week, 30 days): kind, ids, names, short facts; never terminal output, prompts, commands, grader output, paths; token-shaped words are `[redacted]`.
- **While you were away** (back after 10+ min): turns finished, blocked now, fan-outs graded, commands suggested, cost. **Machines**: asked at once (4 s each), merged, labelled; a silent one makes it `partial`.

## Digests

Off until `hub.toml` `[digest] times` is set ([hub-reference](reference.md#hubtoml); Settings > Digest writes that table). `fw hub digest` previews; `--send` sends now.

- **Says**, since the last digest, per workspace: turns finished, agents blocked now, fan-outs graded (winner / leader), PRs red / green / merged, autopilot decisions and gates waiting, queue stalls, spend today and this week (local midnight, Monday); then the top 3 needs-you (gates, longest blocked, red PRs, stalled queues) with links.
- **To** `channels`: `push` (notify.toml; names and counts only, the 3 as ntfy buttons), `browser` (a card; with alerts on, a notification), `plugin` (`digest.ready`). Empty ones are skipped (`quiet_if_empty`). `delivery.channels` lists only what heard it (`unheard`: no browser open, no plugin listening).
- **Once per slot**: local times per date (a skipped 02:30 runs at 03:00, a repeated 01:30 once); `digest.json` under a lock across restarts and `upgrade`. A slot missed while down is sent on start within 2 h, else skipped; a new schedule owes nothing before it. A send claim older than 5 min is dropped even if its pid runs (reused).

## Permissions

Palette **Permissions: FOLDER**, `fw hub perms show`.

- **View**: Claude Code sources highest first (managed `/etc/claude-code` + `managed-settings.d`, the hub's `--settings` + `--permission-mode`, local, project, user) with each rule; Codex trust (`[projects."<repo>"]`), `approval_policy`, `sandbox_mode`, `rules/*.rules`, and their sources.
- **Always allow** (why?, quick answers' `always`, the sheet): the request from the `PermissionRequest` hook, else the dialog on screen, as the narrowest rule (`Bash(cargo test -p app *)`: program, subcommand, scoping flags; else the exact command; one per command of a line).
- Written to `.claude/settings.local.json` (warned when tracked; `--gitignore` ignores it) or, opt-in, user. Preview: file, diff, "This lets the agent ...", an overriding deny/ask. Atomic, keys and layout kept; Yes is typed only if the dialog on screen is that request; the rule is typed back to confirm.
- **Never proposed**: every command, `rm`, `sudo`, `curl`/`wget`, interpreters, `git push` (also `git -C dir push`), `git *`, `cp`/`mv`/`tee`/`sed`/`awk *`, `gh pr merge`, `fw`/`flywheel`, `find *`, exec subcommands (`npm exec`, `git rebase`), `timeout 5 *`, whole MCP servers, pipes into a shell,
  substitutions, `&&` `;` `|`, writes outside the repo or to `.claude/` `.codex/` `.git/` `.gitmodules` `.mcp.json`.
- **Suggestions**: `permission-requests.jsonl` (0600, 30 days; rules, never commands) per repo, most frequent first, minus what a rule covers.
- **Codex**: prefix rules to `.codex/rules/default.rules` (loaded once trusted) or `~/.codex`; `trust_level` in `$CODEX_HOME/config.toml`, `approval_policy` / `sandbox_mode` in `.codex/config.toml`.
  Typed back: `never`, `danger-full-access`, trusting a project whose `.codex/` holds those, a dangerous rule, or `notify`/`mcp_servers`/hooks; a symlinked `.codex` is refused. Writes log and emit `perms.allowed`.

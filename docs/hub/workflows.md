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

`wait`: 0 met, 124 timeout, 1 error (or the session ended). `--changed`: only statuses begun after it started (`wait --changed &`, then prompt). `--until idle` accepts `done`; `exit` prints `exited N` / `killed (SIG)`; `--timeout 0` checks once.

## Agent launcher

- **First prompt** (`--prompt`, 32 KiB): the first user turn, on create only. **Model**: `claude --model` / `codex -m`.
- **Permissions**: Claude `--permission-mode` (`bypassPermissions` warns, marks the session `unchecked`); Codex `-a` approval, `-s` sandbox; reapplied on relaunch and restore.
- **Templates** (per workspace): kind, prompt, model, permissions, worktree; a taken name needs `--replace` (all) or `--edit` (given flags). Launch: palette, **From template**, `new --template NAME` (flags win). Defaults: `[agents.*]`.

## Suggested commands

Agents wrap commands for the screen; the hub reads each fenced block from the agent's own record and serves it **exactly as written**.

- Sources: Claude's transcript (assistant text; before it is on disk, the Stop hook's `last_assistant_message`, so blocks are in before `done`), Codex's rollout (`$CODEX_HOME/sessions/.../rollout-*-<thread-id>.jsonl`), else its `notify` payload. Tailed each second and per hook report; newest 500 per session persist (`commands/<id>.json`).
- Shell blocks: `bash sh zsh shell console fish ksh ...`, or untagged ones that start like a command; `console` keeps only the commands (`$ ` stripped).
- Send pastes (bracketed when enabled), Send + Run adds Enter; multi-line without bracketed paste: 409, use Copy. `--copy`: OSC 52, over SSH too (tmux: `set -g set-clipboard on`; some terminals drop ~75 KB+).
- **Commands drawer** (`Alt C`): newest first; Send targets the companion shell (opened if needed). Keys: `j`/`k`, `Enter`/`c` copy, `1`-`9` that card, `s` send, `Shift S` send + run.

## Usage and cost

- From transcripts (Claude `message.usage` incl. subagents, Codex `token_count`) at list prices by model (`usage.rs` `PRICES`; subscriptions too; unknown models: tokens only). Claude's side calls (titles) are not in transcripts: ~1-2% under its `cost-state`.
- Header meter, sidebar, `fw hub ls`, `GET /api/usage`; 62 days per session; removed ones in `usage-archive.json`.

## Worktrees and companion shells

```bash
fw hub new WS --kind claude --worktree fix-retry [--base main]  # git worktree add -b <prefix>fix-retry ../<repo>.worktrees/fix-retry
fw hub kill fix-retry --rm --remove-worktree  # refused if work would be lost, unless --force
```

- A sibling of the **main** checkout; base `--base`, else the current branch; bad names, existing dirs or branches refused before git runs. Removal: 409 while another session uses it, or (without `--force`) over uncommitted, untracked, or unmerged work; the branch goes only when nothing is lost.
- **Companion shell**: one per agent, in its directory (`Alt \``, the drawer, the API), closed with it; the `shell` / `agent` pill flips.

## Review

Any session in a git folder: pill `review`, `Alt G`, `fw hub review`. Base: the worktree's, else the upstream (`--base`). **Every action is planned first** (exact commands, refusals); running needs the plan's `fingerprint` (repo changed since: 409).

| Action | Runs | Refused when |
|---|---|---|
| Commit all | `git add -A` (but ticked files); `git commit -F -` (hooks run) | what `add` staged differs from the plan |
| Push | `git push <remote> <planned HEAD>:refs/heads/<B>` | no remote; never forced |
| Open PR | `gh pr create --base --head B --body-file -` | not pushed / out of sync, no `gh` auth |
| Rebase onto base | `git rebase <base>` in the worktree | uncommitted changes (conflicts stop; Abort) |
| Merge into base | `git -C <main checkout> merge --ff-only <planned commit>` | main checkout dirty or off base, not a fast-forward |

- **Commit all** checks every changed path, not just the 1000 shown. Ticked: `generated` (`node_modules/`, `__pycache__/`, `.venv/`, `*.egg-info/`, `.pyc`, a dir with `CACHEDIR.TAG` or `pyvenv.cfg`, `target/` by `Cargo.toml`); `secret` (`.env*`, keys by name or content, credentials, `*.tfvars`), `personal` (`.claude/settings.local.json`, `CLAUDE.local.md`), `junk` (`.DS_Store`, `*.log`).
- Unticked: `build` (`dist/`, `build/`, `coverage/`, `venv/`, other Cargo-shaped `target/`), `large` (5 MiB+).
- No git identity: refused before staging; a failed step puts the index back.
- **Merge** and fan-out **pick** refuse a branch that adds a `generated`/`secret`/`personal`/`junk` file unless `--include-suspects` (the dialog's box).
- "Ask the agent" pastes a commit-message or conflict prompt. Runner: argv, no shell or prompts, deadlines, 4 git/gh at once, one action per repo, logged.

## Pull requests and CI

A session whose branch has a pull request: a badge on its row (`#7`, checks), a PR section in Review, `fw hub pr SESSION`. All through `gh`; missing or signed out: nothing is asked, the section says why.

- **Asked** per (folder, branch) by head (`gh pr list --head B`; a branch named `123` never binds PR #123), pushed and not a default branch: checks running 1 min, open 5 min, merged/closed 30 min, none 5 min; at once after Push, Open PR, its own actions; Refresh (5 s apart). At most 30 gh calls/min; a rate limit pauses asking 15 min (known state kept, marked stale).
- **View log**: `gh run view --job J --log-failed`, step headers, the last 400 lines, terminal escapes stripped (also from check names), secrets redacted (Activity's rules, `NAME=value`, `password:`, `--password V`, AWS keys, URL passwords).
- **Ask agent to fix CI**: the failing checks, the log's last 150 lines under `From fw hub: CI failed on pull request #N ...`; pasted at rest, else queued (`done`) behind its queue. One ask per PR waits (newer replaces it), dropped once checks pass or the PR closes; never pasted twice in 5 min.
- **Re-run failed** (`gh run rerun R --failed`), **Ready** (`gh pr ready`), **Merge** (`gh pr merge N --<method> --match-head-commit SHA`, the repo's default method if allowed): planned, confirmed by fingerprint against GitHub read again (unreadable: refused).
- Merge refuses closed, draft, conflicting; checks failing or running, changes requested, review required: only with the number typed back (`--force N`). Branch protection still applies.
- **Events** `pr.checks_failed`, `pr.checks_passed` (from running or red), `pr.review_changes_requested`, `pr.merged`: plugins, Activity, browser alerts. A first look (a hub start) announces nothing.

## Handoff and peer review

- **Send to agent** (`fw hub handoff FROM TO`, `handoff.send`): a selection, message, block, or diff to another agent (any workspace or machine) as one editable prompt (`From <session> on <branch>:` + a fence; 32 KiB).
- **Ask for a review** (`fw hub ask-review`, Review drawer, `review.ask`): a read-only reviewer of the other kind (`review.reviewer`; Claude `plan`, Codex `-a never -s read-only`) in the author's worktree, prompted with `git diff <base>...HEAD`; one per author. Model: `--model`, else the author's (same kind), else `agents.<kind>.model`; plan mode pins an alias (`haiku`) to the author's full id.
- **Send findings to <author>** pastes the reviewer's latest reply back. Pairing survives restarts; the author's close closes its reviewers.

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

- Name: workspace + prompt hash (`demo-3f9a2c`).
- **Agent claims are untrusted**: a finished turn only queues grading; the verdict is the graders' exit codes. Needs you lists it once per unseen grading.
- **Graders are held out of the prompt only** (agents run as you): `--protect` a tracked grader's inputs (a change fails) or keep it outside the repo ([held-out gate](../held-out-gate.md)); absolute-path scripts are pinned by hash.
- Grading: flywheel-core's runner (in order, `/bin/sh -c`, exit 0 passes, `--timeout` 900 s each, first failure stops); 2 at once. Rank: passing (smaller diff, then lower cost), failing, stale, grading, ungraded; a pass changing nothing cannot be picked.
- **Pick** needs a fresh pass on a clean tree at exactly the graded commit, then ff-merges it (plan + fingerprint); base moved: `--rebase`, which grades again.
- **Cleanup** removes losers' worktrees and sessions (unique work: `--force`; `--stop-only` keeps worktrees); `rm` forgets. Max 50; gradings cut short by stop or upgrade rerun.

## Fan-outs and flywheel tasks

For a folder flywheel drives with a **directory** work source:

```bash
fw hub fanout promote "$id" --reference c1  # instead of pick; dry run: path and exact bytes
fw hub fanout promote "$id" --reference c1 --yes  # writes .flywheel/tasks/active/NN-<slug>/<slug>-<tag>.json (uncommitted)
fw hub fanout new demo --from-task fix-retry --agent claude:opus --agent claude:sonnet
```

- **Promote instead of picking** to land via `fw worker` (refused after a pick; a later pick warns): goal (`--goal`, the seed task's, or the prompt's first line), prompt -> `context.notes`, graders -> `command` graders, `--protect` -> constraints (only `[submit] protected_paths` refuses a landing), `--reference` (default winner, else leader); refused for an existing file or id.
- The file is left **uncommitted**; **Run <id> with worker here** runs `fw worker --task <id>` alone.
- **Fan out this task** (Flywheel panel): prompt from its goal and context; graders = its command plus **held-out** graders, added server-side (none: refused; an unreadable registry fails closed). A prompt naming a held-out grader is refused; Ask to fix only says one failed. Not a sandbox.

## Flywheel panel

**Postgres stores read the DSN from the hub's own environment** (`FLYWHEEL_PG_DSN`, else `DATABASE_URL`): start the hub from a shell that exports it.

Folders with a `flywheel.toml` or `.flywheel/` get the verified loop beside the terminals, read-only (workers it starts are hub shells); `--no-flywheel` turns it off.

- Phases and tasks, live runs, the 12 newest runs with grader receipts, worker and autopilot liveness, claims, alerts (**Needs you** until **Mark seen**). Open: the folder chip's `verified/total` badge, `Alt F`, the palette.
- **Start worker here**: `fw worker` in a split below; a task's **run**: `fw worker --task <id>` (asks to add unmet prerequisites); a second asks first (409); `queued in <session>`: a scoped worker will run it. Outcome: `verified`, `not verified`, `refused`, `worker exit N`. **Output**: `fw show` summary and redacted transcript, live.
- **Autopilot** (`fw hub autopilot`): **Start** (confirmed: lands unattended) = `fw autopilot` in a hub session; stop = Ctrl-C; pause / resume = `fw autopilot --pause|--resume`. Peers: read-only.
- Shown: tier verdicts (tap one: its name), the newest deciding cycle (emitted, dropped with reasons), the 20 newest decisions, emitted tasks (score, run), parked gates. **Approve** / **Deny** = `fw approve` / `fw reject` pinned to the gate shown (confirmed; one per gate, then `decision pending`, off Needs you; deny retries with feedback). Held-out gate, risk tiers, team reviews: shown.
- A linked worktree reads its own store; `github*` sources are not listed; a missing SQLite store is not created. One watcher per folder (stat 2 s, re-read 15 s). Seam: `flywheel::FlywheelProvider`, implemented in `hub_flywheel.rs`.

## Prompt queue

Per agent session, a FIFO of prompts the hub types in itself ("when it's done, then do X"), kept by the hub owning the session.

`fw hub queue add api "run the tests"`, `fw hub queue add api --when idle "now the changelog"`, `fw hub queue ls api`.

| | |
|---|---|
| Triggers (`--when`) | `done` (default): at rest (turn finished, or awaiting a first prompt); `idle`: at rest and viewed; `blocked-cleared`: at the front, blocked, answered, finished |
| Never | `working`, `blocked`, `unknown`; within 1.5 s of the rest or `queue.typing_quiet` of any typing; a grading, handoff, review, or edit holds it |
| Drafts | text left in the agent's input line holds it (send or clear it); keys answering a prompt do not count; nothing lands between a delivery's text and its Enter |
| One at a time | the next waits for a rest begun after the last delivery (bracketed paste, then Enter); the newest 20 delivered stay |
| Exactly once | `sending` saved before the paste, `delivered` after; a crash between: the transcript decides (`reconciled`) or it is pending again; `upgrade` carries typing times and drafts |
| Stalls | until resumed or relaunched: the agent exited or is a shell, the paste was refused, or no turn started in 120 s (`queue.stalled`) |
| Order | drag, up/down, `queue move`; pending editable; 50 pending, 64 KiB each |

## Activity

What happened, newest first: palette **Activity**, the sidebar chip, the header pill.

`fw hub activity` (24 h: the digest, then the timeline); `--since last-seen` (since this CLI last looked; marks now); `--since 2h --workspace api --kind review --json`.

- **Rows**: the plugin events (`status.changed` only to `blocked`), `turn.finished`, `commands.suggested`, `usage`, `plugin.ran`, `hub.stopped`. A row opens its object, else attaches.
- **Kept** in `activity.jsonl` (rotated at 1 MiB or a week, `activity.1..5.jsonl`, 30 days): kind, ids, names, short facts; never terminal output, prompts, command text, grader output, paths; token-shaped words (long hex too) are `[redacted]`, machines' too.
- **While you were away** (back after 10+ min): turns finished, blocked now, fan-outs graded, commands suggested, cost. **Machines**: asked at once (4 s each), merged, labelled; a silent one makes it `partial`.

## Permissions

Palette **Permissions: FOLDER**, `fw hub perms show`.

- **View**: Claude Code sources highest first (managed `/etc/claude-code` + `managed-settings.d`, the hub's `--settings` + `--permission-mode`, local at the main checkout's root, project, user), each rule and file; Codex trust (`[projects."<repo>"]`), `approval_policy`, `sandbox_mode` and their sources, `rules/*.rules`.
- **Always allow** (why?, quick answers' `always`, the sheet): the request from the `PermissionRequest` hook, else the dialog on screen; the narrowest rule (`Bash(cargo test -p app *)`: program, subcommand, scoping flags; else the exact command; one per command of a line).
- Default file `.claude/settings.local.json` (warned when tracked; untracked-not-ignored: `--gitignore` / the box adds it); user is opt-in. Preview: file, diff, "This lets the agent ...", an overriding deny/ask. Atomic, serialized, keys and layout kept; Yes is typed only if the dialog on screen is that request.
- **Never proposed**: every command, `rm`, `sudo`, `curl`/`wget`, interpreters, `git push` (also `git -C dir push`), `git *`, `cp`/`mv`/`tee`/`sed`/`awk *`, `gh pr merge`, `fw`/`flywheel`, `find *`, exec subcommands (`npm exec`, `git rebase`), `timeout 5 *`, whole MCP servers, pipes into a shell, substitutions, writes outside the repo or to `.claude/` `.codex/` `.git/` `.gitmodules` `.mcp.json`.
- Writing one takes the rule typed back (CLI: on a terminal). `&&` `;` `|` refused.
- **Suggestions**: `permission-requests.jsonl` (0600, 30 days; rules, never commands) per repo, most frequent first, minus what a rule covers.
- **Codex**: prefix rules to `.codex/rules/default.rules` (loaded once trusted) or `~/.codex`; `trust_level` in `$CODEX_HOME/config.toml`, `approval_policy` / `sandbox_mode` in `.codex/config.toml`; `never` and `danger-full-access` are typed back. Writes: a hub log line and `perms.allowed`.
- Trusting a project whose `.codex/` holds those, a dangerous rule, or `notify`/`mcp_servers`/hooks is typed back too (listed); a symlinked project `.codex` is refused.

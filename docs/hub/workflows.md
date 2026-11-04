# Hub workflows

Driving agents from scripts and the UI. Front door: [hub](../hub.md).

## Scripting agents

```bash
id=$(fw hub new demo --kind claude --name fixer)
fw hub wait "$id" --until idle --timeout 60           # agent up, waiting for a prompt
fw hub send "$id" $'Fix the failing test in src/lib.rs\n'
fw hub wait "$id" --until done --changed --timeout 900 || { fw hub read "$id"; exit 1; }
fw hub commands "$id" --last > next.sh                # newest shell block it suggested, exactly
fw hub send sh "$(cat next.sh)" --enter               # run it in a shell session
fw hub new demo --name build -- sh -c "$(cat next.sh)" && fw hub wait build --until exit --timeout 600
fw hub wait "$id" --until blocked --timeout 5 && fw hub read "$id"   # 0: it waits on a prompt
```

- `wait`: 0 met, 124 timeout, 1 error (or the session ended first). `--changed`: only statuses begun after it started (`wait --changed &`, then the prompt). `--until idle` accepts `done`; `exit` prints `exited N` / `killed (SIG)`; `--timeout 0` checks once.

## Agent launcher

- **First prompt** (`--prompt`, 32 KiB): the first user turn, on create only. **Model**: `claude --model` / `codex -m`.
- **Permissions**: Claude `--permission-mode` (`bypassPermissions` warns and marks the session `unchecked`); Codex `-a` approval, `-s` sandbox; reapplied on relaunch and restore.
- **Templates** (per workspace, agents): kind, prompt, model, permissions, worktree; an existing name needs `--replace` (all) or `--edit` (flags given). Launch: palette, **From template**, `new --template NAME` (flags win). Defaults: `hub.toml` `[agents.*]`.

## Suggested commands

Agents wrap commands for the screen; the hub reads each fenced block from the agent's own record and serves it **exactly as written**.

- Sources: Claude's transcript (assistant text only), Codex's rollout (`$CODEX_HOME/sessions/.../rollout-*-<thread-id>.jsonl`), else its `notify` payload.
- Tailed every second and per hook report; the newest 500 per session persist (`commands/<id>.json`).
- Shell blocks: `bash sh zsh shell console fish ksh ...`, or untagged ones that start like a command; `console` blocks keep only the commands (`$ ` stripped).
- Send pastes (bracketed when enabled), Send + Run adds Enter; multi-line without bracketed paste: 409, use Copy.
- `fw hub commands S --copy`: OSC 52, over SSH too (tmux: `set -g set-clipboard on`; some terminals drop copies over ~75 KB).
- **Commands drawer** (`Alt C`): newest first; Send targets the companion shell (opened if needed). Keys: `j`/`k`, `Enter`/`c` copy, `1`-`9` that card, `s` send, `Shift S` send + run.

## Usage and cost

- From transcripts (Claude `message.usage` incl. subagents, Codex `token_count`), priced by model: `usage.rs` `PRICES` (list prices, also on subscriptions; unknown models: tokens only).
- Header meter, sidebar rows, `fw hub ls`, `GET /api/usage`; 62 days per session; removed sessions' in `usage-archive.json`.

## Worktrees and companion shells

```bash
fw hub new WS --kind claude --worktree fix-retry [--base main]   # git worktree add -b <prefix>fix-retry ../<repo>.worktrees/fix-retry
fw hub kill fix-retry --rm --remove-worktree                     # refused if work would be lost, unless --force
```

- A sibling of the **main** checkout; base `--base`, else the current branch; bad names, existing dirs or branches refused before git runs.
- Removal: 409 while another session uses it, or (without `--force`) over uncommitted, untracked, or unmerged work; the branch goes only when nothing is lost.
- **Companion shell**: one per agent, in its directory (`Alt \``, the drawer, the API), closed with it; the `shell` / `agent` pill flips between them.

## Review

Any session in a git folder: pill `review`, `Alt G`, `fw hub review`. Base: the worktree's, else the upstream (`--base`). Commits ahead, files vs the merge base, per-file diffs.

- **Every action is planned first** (exact commands, refusals); running needs the plan's `fingerprint`; a repo changed since: 409.

| Action | Runs | Refused when |
|---|---|---|
| Commit all | `git add -A`; `git commit -F -` (hooks run) | what `add` staged differs from the plan |
| Push | `git push <remote> <planned HEAD>:refs/heads/<B>` | no remote; never forced |
| Open PR | `gh pr create --base --head B --body-file -` | not pushed / out of sync, no `gh` auth for the host |
| Rebase onto base | `git rebase <base>` in the worktree | uncommitted changes (a conflict stops there; Abort) |
| Merge into base | `git -C <main checkout> merge --ff-only <planned commit>` | main checkout dirty or off base, not a fast-forward |

- "Ask the agent" pastes a commit-message or conflict prompt. Runner: argv, no shell or prompts, deadlines, 4 git/gh at once, one action per repo, logged.

## Handoff and peer review

- **Send to agent** (`fw hub handoff FROM TO`, `handoff.send`): a selection, message, block, or diff to another agent (any workspace or machine) as one prompt (`From <session> on <branch>:` + a fence; 32 KiB, editable).
- **Ask for a review** (`fw hub ask-review`, Review drawer, `review.ask`): a read-only reviewer of the other kind (`review.reviewer`; Claude `plan`, Codex `-a never -s read-only`) in the author's worktree, prompted with `git diff <base>...HEAD`; one per author, reused.
- **Send findings to <author>** pastes the reviewer's latest reply back. Pairing survives restarts; closing the author closes its reviewers.

## Verified fan-out

One prompt to 2-6 agents, each in its own sibling worktree from **the same base commit**, graded by commands the **hub** runs.

```bash
id=$(fw hub fanout new demo --prompt "Fix the flaky retry test" \
  --agent claude:opus --agent claude:sonnet --agent codex:gpt-5.1-codex \
  --grader 'unit=cargo test -p retry' --grader ~/oracles/retry-check.sh --permission-mode acceptEdits)
fw hub fanout wait "$id" --timeout 3600; fw hub fanout show "$id" -v
fw hub fanout fix "$id" c3 --yes           # paste c3's failing grader output into its agent
fw hub fanout pick "$id" c1 --yes [--rebase]
fw hub fanout cleanup "$id" [--force] --yes
```

- **Agent claims are untrusted**: a finished turn only queues grading; the verdict is the graders' exit codes. Needs you lists the fan-out once per unseen grading.
- **Graders are held out of the prompt only** (agents run as you). A tracked grader is editable: `--protect` its inputs (changing one fails) or keep the oracle outside the repo ([held-out gate](../held-out-gate.md)); absolute-path scripts are pinned by hash.
- Grading: flywheel-core's runner (in order, `/bin/sh -c`, exit 0 passes, `--timeout` 900 s each, first failure stops); 2 at once.
- Ranking: passing (smaller diff, then lower cost), failing, stale, grading, ungraded. A pass that changes nothing cannot be picked (the graders pass on the base).
- **Pick** needs a fresh pass on a clean tree at exactly the graded commit, then ff-merges it (plan + fingerprint); base moved: `--rebase`, which grades again.
- **Cleanup** removes losers' worktrees and sessions (unique work: `--force`; `--stop-only` keeps worktrees); `rm` forgets. Max 50; gradings cut short by stop or upgrade rerun.

## Fan-outs and flywheel tasks

For a folder flywheel drives with a **directory** work source:

```bash
fw hub fanout promote "$id"            # dry run: path and exact bytes
fw hub fanout promote "$id" --yes      # writes .flywheel/tasks/active/NN-<slug>/<slug>-<tag>.json (uncommitted)
fw hub fanout new demo --from-task fix-retry --agent claude:opus --agent claude:sonnet
```

- **Promote**: goal (`--goal`, the seed task's, or the prompt's first line), prompt -> `context.notes`, graders -> `command` graders, `--protect` -> constraints (only `[submit] protected_paths` refuses a landing), winner -> a reference; refused for an existing file or id.
- The file is left **uncommitted**; **Run <id> with worker here** runs `fw worker --task <id>` (that task alone).
- **Fan out this task** (Flywheel panel): prompt from its goal and context; graders = its command plus **held-out** graders, added server-side (none: refused; an unreadable registry fails closed).
- Held-out graders never reach an agent via the hub: a prompt naming one is refused; Ask to fix only says one failed. Not a sandbox. A task-seeded fan-out offers **Run <id> with worker here** at once.

## Flywheel panel

**Postgres stores read the DSN from the hub's own environment** (`FLYWHEEL_PG_DSN`, else `DATABASE_URL`): start the hub from a shell that exports it.

Folders with a `flywheel.toml` or `.flywheel/` get the verified loop beside the terminals. It reads the store, never writes it (workers it starts are hub shells); `--no-flywheel` turns it off.

- Phases and tasks, live runs, the 12 newest runs with grader receipts, worker and autopilot liveness, claims, alerts (in **Needs you** until **Mark seen**). Opened from the folder chip's `verified/total` badge, `Alt F`, or the palette.
- **Start worker here**: a hub shell running `fw worker` in a split below; a task's **run**: `fw worker --task <id>` (asks before adding unmet prerequisites). A second needs confirming (409). Outcome: `verified`, `not verified`, `refused`, `worker exit N`.
- **Output** on a run: its `fw show` summary and redacted transcript, followed live.
- A linked worktree reads its own store; `github*` sources are not listed; a missing SQLite store is never created. One watcher per folder (stat 2 s, re-read 15 s).
- Seam: `flywheel-hub` defines `flywheel::FlywheelProvider`; the `flywheel` crate implements it (`hub_flywheel.rs`).

## Prompt queue

Per agent session, a FIFO of prompts the hub types in itself ("when it's done, then do X"), kept by the hub owning the session.

```bash
fw hub queue add api "run the tests and fix what fails"
fw hub queue add api --when idle "now write the changelog entry"
fw hub queue ls api
```

- **Triggers** (`--when`): `done` (default): at rest (turn finished, or awaiting a first prompt); `idle`: at rest and the turn was viewed; `blocked-cleared`: once at the front, the agent blocked, was answered, and finished.
- **Never** while `working`, `blocked`, `unknown`; within 1.5 s of the rest; within `queue.typing_quiet` of any typing (terminal, `attach`, `send`, Compose, pastes); while a grading, handoff, or review holds it or the item is edited.
- **Unsent drafts** hold it: text left in the agent's input line (send or clear it); keys answering a blocked prompt do not count. No paste lands between a delivery's text and its Enter.
- **One at a time**: the next waits for a rest begun after the last delivery (bracketed paste, then Enter); the newest 20 delivered stay.
- **Exactly once**: `sending` saved before the paste, `delivered` after; a crash between: the transcript decides (`reconciled`) or it is pending again. `upgrade` carries typing times and drafts.
- **Stalls** stop it until resumed or relaunched: the agent exited or came back a shell, the paste was refused, or a delivery started no turn in 120 s (`queue.stalled`).
- **Order**: drag, up/down, `queue move`; pending items editable. 50 pending, 64 KiB each.

## Activity

What happened, newest first: palette **Activity**, the sidebar chip, the header pill.

```bash
fw hub activity                         # last 24 h: the digest, then the timeline
fw hub activity --since last-seen       # since this CLI last looked (marks now)
fw hub activity --since 2h --workspace api --kind review --json
```

- **Rows**: the plugin events (`status.changed` only to `blocked`), `turn.finished`, `commands.suggested`, `usage`, `plugin.ran`, `hub.stopped`. A row opens its object, else attaches.
- **Kept** in `activity.jsonl`: rotated at 1 MiB or a week (`activity.1..5.jsonl`), 30 days. Only kind, ids, names, short facts: never terminal output, prompts, command text, grader output, paths; token-shaped words (long hex too) are `[redacted]`, in machines' rows as well.
- **While you were away**: a browser back after 10+ min gets one card: turns finished, blocked now, fan-outs graded, commands suggested, cost.
- **Machines**: asked at once (4 s each), merged, labelled; one not answering makes it `partial`.

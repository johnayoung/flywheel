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

- `wait` listens on the events socket: 0 met, 124 timeout, 1 error (also when the session ends first). `--changed` counts only statuses begun after it started (so `wait --changed &` then a prompt never misses a fast turn). `--until idle` accepts `done`; `--until exit` prints `exited N` / `killed (SIG)`; `--timeout 0` checks once.

## Agent launcher

The new-session form and `fw hub new` take:

- **First prompt** (`--prompt`, max 32 KiB): the CLI's first user turn; on create only. **Model**: `claude --model` / `codex -m`, any id.
- **Permissions**: Claude `--permission-mode` (`bypassPermissions` warns and marks the session `unchecked`); Codex `-a` approval, `-s` sandbox; reapplied on relaunch and restore.
- **Templates** (per workspace, agents only): kind, prompt, model, permissions, worktree. `template save` refuses an existing name unless `--replace` (all options) or `--edit` (only the flags given).
- Launch one from the palette, the sidebar's **From template**, or `new --template NAME` (flags win). Defaults for launches naming none: `hub.toml` `[agents.*]`.

## Suggested commands

Agents wrap commands for the screen, so copying them off the terminal breaks them; the hub reads each fenced block from the agent's own record and serves it **exactly as written**.

- Sources: Claude's transcript (assistant text only), Codex's rollout (`$CODEX_HOME/sessions/.../rollout-*-<thread-id>.jsonl`), else its `notify` payload.
- Tailed every second and per hook report; the newest 500 per session persist (`commands/<id>.json`) across restarts and `/clear`.
- Shell blocks: `bash sh zsh shell console fish ksh ...`, or untagged ones that start like a command; `console` blocks keep only the commands (`$ ` stripped).
- Send pastes (bracketed when enabled), Send + Run adds Enter; a multi-line paste without bracketed paste is refused (409): use Copy.
- `fw hub commands S --copy`: OSC 52, over SSH too (tmux: `set -g set-clipboard on`; some terminals drop copies over ~75 KB).
- **Commands drawer** (`Alt C`): the agent's blocks newest first with Copy, Send, Send + Run. Send targets its companion shell (else the attached one), opened if needed. Keys: `j`/`k`, `Enter`/`c` copy, `1`-`9` copy that card, `s` send, `Shift S` send + run.

## Usage and cost

- From transcripts (Claude `message.usage` incl. subagents, Codex `token_count`), priced by the recorded model: `usage.rs` `PRICES` (Anthropic list prices 2026-10-06, OpenAI GPT-5 2025-11). Unknown models: tokens only; no server-tool fees; list prices even on subscriptions.
- Header meter, sidebar rows, `fw hub ls`, `GET /api/usage`. 62 days of daily totals per session; a removed session's move to `usage-archive.json`.

## Worktrees and companion shells

```bash
fw hub new WS --kind claude --worktree fix-retry [--base main]   # git worktree add -b <prefix>fix-retry ../<repo>.worktrees/fix-retry
fw hub kill fix-retry --rm --remove-worktree                     # refused if work would be lost, unless --force
```

- A sibling of the **main** checkout; base `--base`, else the current branch. A bad task name, existing dir or branch is refused before git runs; a failed launch removes it.
- Branch badges refresh on status changes, after Send + Run, and every 20 s.
- Removal is refused (409) while another session uses it, or without `--force` over uncommitted, untracked, or unmerged work; the branch goes only when nothing would be lost.
- **Companion shell**: one per agent, in its directory (`Alt \``, the drawer, the API), closed with it; the `shell` / `agent` pill flips between them.

## Review

Any session in a git folder: header pill `review`, `Alt G`, or `fw hub review`. A drawer on desktop, a sheet on phones.

- Base: the worktree's base, else the upstream; `--base` overrides.
- Commits ahead, files vs the merge base, the working tree; per-file diffs, unified or side by side.
- **Every action is planned first** (exact commands, where, any refusal); running needs that plan's `fingerprint`, and a repo changed since is refused (409).

| Action | Runs | Refused when |
|---|---|---|
| Commit all | `git add -A`; `git commit -F -` (hooks run) | what `add` staged differs from the plan |
| Push | `git push <remote> <planned HEAD>:refs/heads/<B>` | never forced; a diverged remote is git's refusal |
| Open PR | `gh pr create --base --head B --body-file -` | not pushed / out of sync, no `gh` auth for the host |
| Rebase onto base | `git rebase <base>` in the worktree | uncommitted changes (a conflict stops there; Abort) |
| Merge into base | `git -C <main checkout> merge --ff-only <planned commit>` | main checkout dirty or off base, not a fast-forward |

- "Ask the agent" pastes a commit-message or conflict prompt into its agent. Runner: argv, no shell or prompts (`GIT_TERMINAL_PROMPT=0`), deadlines kill the group, 4 git/gh at once, one action per repo, all logged.

## Handoff and peer review

- **Send to agent** (`fw hub handoff FROM TO`, palette, `handoff.send`): a selection, transcript message, block, or diff to another agent (any workspace or machine) as one prompt (`From <session> on <branch>:` + one fence; 32 KiB default, editable).
- **Ask for a review** (`fw hub ask-review`, Review drawer, `review.ask`): a read-only reviewer of the other kind (`review.reviewer`; Claude `plan`, Codex `-a never -s read-only`) in the author's worktree, prompted with `git diff <base>...HEAD`; one per author, reused.
- Findings: the reviewer's latest reply and code blocks; **Send findings to <author>** pastes them back. Pairing survives restarts; closing the author closes its reviewers. Events: `review.requested`, `handoff.sent`.

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

- **Agent claims are untrusted**: a finished turn only queues grading; the verdict is the graders' exit codes. Needs you lists the fan-out once per unseen grading, not its candidates ([hub-internals: Agent status](internals.md#agent-status)).
- **Graders are held out of the prompt only** (agents run as you). A tracked grader is editable: `--protect` its inputs (changing one fails) or keep the oracle outside the repo ([held-out gate](../held-out-gate.md)); absolute-path scripts are pinned by hash.
- Grading = flywheel-core's command-grader runner (in order, `/bin/sh -c`, pass = exit 0, `--timeout` 900 s each, first failure stops, 8 KiB tails); 2 at once hub-wide.
- Ranking: passing (smaller diff, then lower cost), failing, stale, grading, ungraded. A pass that changes nothing cannot be picked (the graders pass on the base).
- **Pick** needs a fresh pass on a clean tree at exactly the graded commit, then ff-merges it (plan + fingerprint); base moved: `--rebase`, which grades again.
- **Cleanup** removes losers' worktrees and sessions (unique work only with `--force`; `--stop-only` keeps worktrees); `rm` forgets the record. Max 50, this hub's only; gradings cut short by stop or upgrade rerun.

## Fan-outs and flywheel tasks

For a folder flywheel drives with a **directory** work source:

```bash
fw hub fanout promote "$id"            # dry run: path and exact bytes
fw hub fanout promote "$id" --yes      # writes .flywheel/tasks/active/NN-<slug>/<slug>-<tag>.json (uncommitted)
fw hub fanout new demo --from-task fix-retry --agent claude:opus --agent claude:sonnet
```

- **Promote**: goal (`--goal`, the seeding task's, or the prompt's first line), prompt -> `context.notes`, graders -> `command` graders, `--protect` -> constraints, the winner's commit -> a reference; via core's task loader, refused for an existing file, id, or an id with runs.
- `--protect` becomes constraints only: only flywheel.toml `[submit] protected_paths` refuses a landing, and the hub never edits flywheel.toml.
- The file is left **uncommitted** in the main checkout; **Run <id> with worker here** runs `fw worker --task <id>` (that task alone; the dialog lists what stays queued).
- **Fan out this task** (Flywheel panel): prompt from the task's goal and context; graders = its command plus **held-out** graders, added server-side. No command grader: refused; an unreadable held-out registry fails closed.
- Held-out graders never reach an agent via the hub: a prompt naming one is refused; Ask to fix only says one failed (and is not offered for a changed pinned script). Not a sandbox.
- A task-seeded fan-out offers **Run <id> with worker here** at once (already durable); a new task needs `--task-id`.

## Flywheel panel

**Postgres stores read the DSN from the hub's own environment** (`FLYWHEEL_PG_DSN`, else `DATABASE_URL`): start the hub from a shell that exports it.

Workspace folders with a `flywheel.toml` or `.flywheel/` get the verified loop beside the terminals. Read-only; `--no-flywheel` turns it off.

- Phases and tasks (`fw status --json` states), live runs, the 12 newest runs with grader receipts, worker and autopilot liveness, claims, alerts (in **Needs you** until **Mark seen**, per browser).
- Opened from the folder chip's `verified/total` badge, the `flywheel` pill (`Alt F`), or the palette.
- **Start worker here**: a hub shell running `fw worker` (the whole queue) in a split below the focused pane; a task's **run** starts `fw worker --task <id>` (asks before adding unmet prerequisites). A second needs confirming (409). A finished worker shows its outcome (`verified`, `not verified`, `refused`, `worker exit N`).
- **Output** on a run: its `fw show` summary and redacted transcript, followed live.
- A linked worktree reads its own store; `github*` sources are not listed. One watcher per folder (stat 2 s, re-read on change or 15 s); a missing SQLite store is never created.
- Seam: `flywheel-hub` defines `flywheel::FlywheelProvider`, the `flywheel` crate implements it (`hub_flywheel.rs`), so the hub never depends on the loop's crates.

## Prompt queue

Per agent session, a FIFO of prompts the hub types in itself: "when it's done, then do X". Kept by the hub owning the session; `--machine` and the UI reach others through the proxy.

```bash
fw hub queue add api "run the tests and fix what fails"
fw hub queue add api --when idle "now write the changelog entry"
fw hub queue ls api
```

- Add from Compose (**Queue**), the drawer (`Alt Q`), the palette, or `queue add`.
- **Triggers** (`--when`): `done` (default): at rest (turn finished, or awaiting a first prompt); `idle`: at rest and the turn was viewed; `blocked-cleared`: once at the front, the agent blocked, was answered, and finished.
- **Never** while `working`, `blocked`, or `unknown`; within 1.5 s of the rest starting; within `queue.typing_quiet` of any typing (terminal, `attach`, `send`, Compose, hub pastes; not focus reports); while a grading, handoff, or review action holds it or the item is being edited.
- **Unsent drafts** hold it however long: text left in the agent's input line (send or clear it: Ctrl-U, Backspace, Esc Esc); keys answering a blocked prompt do not count. No other paste lands between a delivery's text and its Enter.
- **One at a time**: the next waits for a rest begun after the last delivery (bracketed paste, then Enter); the newest 20 delivered stay.
- **Exactly once**: `sending` is saved before the paste, `delivered` after; after a crash between, the transcript decides (`delivered`, `reconciled`) or it is pending again. `fw hub upgrade` finishes one under way, starts none until the takeover, and carries typing times and drafts.
- **Stalls** stop it until resumed (`queue resume`, the drawer, Needs you) or relaunched: the agent exited or came back a shell, the paste was refused, or a delivered prompt started no turn in 120 s (`queue.stalled`). Removing a session drops its queue.
- **Order**: drag, up/down, `queue move`; pending items can be edited or removed (not one being sent). 50 pending, 64 KiB each. Events: `queue.delivered`, `queue.stalled` ([hub-api: Plugin events](api.md#plugin-events)).

## Activity

What happened, newest first: palette **Activity**, the sidebar's `activity` chip, the header's `activity` pill (phones: its icon).

```bash
fw hub activity                         # last 24 h: the digest, then the timeline
fw hub activity --since last-seen       # since this CLI last looked (marks now)
fw hub activity --since 2h --workspace api --kind review --json
```

- **Rows**: the plugin events (`status.changed` only to `blocked`), `turn.finished`, `commands.suggested`, `usage` (tokens, cost), `plugin.ran`, `hub.stopped`. A row opens its object (transcript, Commands, Review, fan-out board, queue, replay), else attaches.
- **Kept** in `activity.jsonl`: rotated at 1 MiB or a week (`activity.1..5.jsonl`), 30 days (6 MiB at most); survives restarts and `upgrade`. Only kind, ids, names, short facts (status, branch, counts, cost): never terminal output, prompts, command text, grader output, paths; token-shaped words are `[redacted]`.
- **While you were away**: a browser (per-device id) back after 10 min or more gets one dismissible card: turns finished, blocked now, fan-outs graded (winner), commands suggested, cost.
- **Machines**: asked at once (4 s each), merged and machine-labelled; one not answering is named and the answer is `partial`.

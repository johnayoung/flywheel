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

- `wait` listens on the events socket (no polling). Exit 0 met, 124 timeout, 1 error (an agent wait also fails if the session ends first).
- `--changed` ignores the state current at start but counts any status begun after the wait process started, so `wait --changed &` then a prompt cannot miss a fast turn.
- `--until idle` also accepts `done`; `--until exit` prints `exited N` / `killed (SIG)`. `--timeout 0` checks once.

## Agent launcher

The new-session form and `fw hub new` take:

- **First prompt** (`--prompt`, max 32 KiB): passed after `--` as the CLI's first user turn. Only on create; relaunch and restore never re-send it.
- **Model**: `claude --model` / `codex -m`; any id is accepted.
- **Permissions**: Claude `--permission-mode` (`bypassPermissions` warns everywhere and marks the session `unchecked`); Codex `-a` approval and `-s` sandbox. Reapplied on relaunch and restore.
- **Templates** (per workspace, agents only): kind, prompt, model, permissions, worktree yes/no. `fw hub template save` refuses an existing name unless `--replace` (all options) or `--edit` (only the flags given).
- Launch from a template: palette "New agent from template: NAME", the sidebar's **From template**, or `new --template NAME` (its flags override the template's).
- Defaults for launches naming none: `hub.toml` `[agents.*]`.

## Suggested commands

Agents wrap commands for the screen, so copying them off the terminal breaks them. The hub reads each fenced code block from the agent's own record and serves it **exactly as written**.

- Sources: Claude's transcript (assistant text only) and Codex's rollout (`$CODEX_HOME/sessions/.../rollout-*-<thread-id>.jsonl`), else Codex's `notify` payload.
- Tailed every second and on each hook report; the newest 500 blocks per session persist in `<state-dir>/commands/<id>.json` across restarts and `/clear`.
- Shell blocks: `bash sh zsh shell console fish ksh ...`, or untagged blocks that start like a command. In `console` blocks `command` keeps only the commands (`$ ` stripped, output dropped).
- Send (paste) is bracketed when the program enabled it; Enter only with Send + Run. A multi-line paste into a terminal without bracketed paste is refused (409): use Copy.
- `fw hub commands S --copy`: OSC 52 to your terminal, works over SSH (tmux needs `set -g set-clipboard on`; some terminals drop copies over ~75 KB).
- **Commands drawer** (`Alt C`): the attached agent's blocks newest first, each with Copy, Send, Send + Run. It overlays the terminal, never resizing the PTY.
- Send goes to the agent's companion shell by default (else the attached shell); with none the buttons open one first. Keys: `j`/`k`, `Enter`/`c` copy, `1`-`9` copy that card, `s` send, `Shift S` send + run.

## Usage and cost

- Counted from transcripts: Claude's `message.usage` (subagents included), Codex's `token_count`. Cost uses the model the transcript records.
- Prices: embedded table (`crates/flywheel-hub/src/usage.rs` `PRICES`; Anthropic list prices as of 2026-10-06, OpenAI GPT-5 prices 2025-11). Unknown models show tokens only, never a guessed price. Server-tool fees are not counted.
- Costs are API list-price equivalents, including for subscription plans.
- Shown in the header meter, sidebar rows, `fw hub ls [--json]`, `GET /api/usage` (hub and workspace `today` / `week` / `total`).
- Kept with the session's commands file (62 days of daily totals); a removed session's usage moves to `usage-archive.json`.

## Worktrees and companion shells

```bash
fw hub new WS --kind claude --worktree fix-retry [--base main]   # git worktree add -b <prefix>fix-retry ../<repo>.worktrees/fix-retry
fw hub kill fix-retry --rm --remove-worktree                     # refused if work would be lost, unless --force
```

- The worktree is always a sibling of the **main** checkout; base = `--base`, else the current branch. A bad task name, existing dir, or existing branch is refused before git runs; a failed launch removes the worktree.
- Branch badges (branch, ahead/behind vs base or upstream, changed, untracked) are refreshed in the background: on status changes, after Send + Run, and every 20 s.
- Removing the worktree is refused (409) while another session uses it, or, without `--force`, with uncommitted changes, untracked files, or commits its base lacks. The branch is deleted only when nothing would be lost.
- **Companion shell**: one per agent, in its directory (`Alt \``, the drawer, `POST /api/sessions/{id}/companion`). Closing the agent closes it. The header's `shell` / `agent` pill flips between them.

## Review

Any session in a git folder: header pill `review`, `Alt G`, or `fw hub review`. A drawer on desktop (never resizes the PTY), a sheet on phones.

- Base: the worktree's base, else the upstream; `--base` overrides.
- Shows commits ahead, committed files vs the merge base, and the working tree (conflicts, staged, unstaged, untracked). Per-file diffs, unified or side by side.
- **Every action is planned first**: the exact commands, where they run, and any refusal. Running needs that plan's `fingerprint`; a repo that changed since is refused (409, re-plan).

| Action | Runs | Refused when |
|---|---|---|
| Commit all | `git add -A`; `git commit -F -` (hooks run) | what `add` staged differs from the plan |
| Push | `git push <remote> <planned HEAD>:refs/heads/<B>` | never forced; a diverged remote is git's refusal |
| Open PR | `gh pr create --base --head B --body-file -` | not pushed / out of sync, no `gh` auth for the host |
| Rebase onto base | `git rebase <base>` in the worktree | uncommitted changes (a conflict stops there; Abort) |
| Merge into base | `git -C <main checkout> merge --ff-only <planned commit>` | main checkout dirty or off base, not a fast-forward |

- "Ask the agent" pastes a prompt for a commit message or conflict resolution into the session's agent.
- Runner: argv only, no shell, no prompts (`GIT_TERMINAL_PROMPT=0`), deadlines kill the group, 4 git/gh at once hub-wide, one action per repo. Every action is logged.

## Handoff and peer review

- **Send to agent** (`fw hub handoff FROM TO`, palette, `handoff.send`): a terminal selection, transcript message, suggested block, or diff goes to another agent session (any workspace or machine) as one prompt: a `From <session> on <branch>:` header, then the content in one fence. Cut at 32 KiB by default; editable before sending.
- **Ask for a review** (`fw hub ask-review`, Review drawer, `review.ask`): a reviewer of the other kind (`review.reviewer`), read-only (Claude `plan`, Codex `-a never -s read-only`), in the author's worktree, prompted with `git diff <base>...HEAD`. One reviewer per author; asking again reuses it.
- Findings: the reviewer's latest reply with its code blocks; **Send findings to <author>** pastes them back.
- Pairing (`reviewer` / `review_of`) survives restarts; closing the author closes its reviewers. Events: `review.requested`, `handoff.sent`.

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

- **Agent claims are untrusted**: a finished turn only queues grading; the verdict is the graders' exit codes.
- **Graders are held out of the prompt only**: agents run as you and could read them. A grader the repo tracks is editable by the agent: list its inputs under `--protect` (a candidate changing one fails) or keep the oracle outside the repo ([held-out gate](../held-out-gate.md)). Absolute-path scripts are pinned by hash at launch.
- Grading = flywheel-core's command-grader runner: in order, `/bin/sh -c`, pass = exit 0, `--timeout` 900 s each, first failure stops, 8 KiB output tails kept. 2 gradings at once hub-wide.
- Ranking: passing (smaller diff, then lower cost), then failing, stale, grading, ungraded. A pass that changes nothing cannot be picked (the graders pass on the base).
- **Pick** needs a fresh pass on a clean tree at exactly the graded commit, then ff-merges it (plan + fingerprint). Base moved: `--rebase`, which grades again.
- **Cleanup** removes the losers' worktrees and sessions, refusing ones holding unique work unless `--force`; `--stop-only` keeps worktrees. `rm` forgets the record only.
- Kept in `state.json` (max 50). Gradings cut short by stop or upgrade rerun in the hub that carries on. This hub's fan-outs only.

## Fan-outs and flywheel tasks

For a folder flywheel drives with a **directory** work source:

```bash
fw hub fanout promote "$id"            # dry run: path and exact bytes
fw hub fanout promote "$id" --yes      # writes .flywheel/tasks/active/NN-<slug>/<slug>-<tag>.json (uncommitted)
fw hub fanout new demo --from-task fix-retry --agent claude:opus --agent claude:sonnet
```

- **Promote**: goal (`--goal`, else the seeding task's, else the prompt's first line), prompt -> `context.notes`, graders -> `command` graders, `--protect` -> constraints, the winner's commit -> a reference. Written through core's task loader; refused for an existing file, id, or an id with runs.
- `--protect` paths become constraints only: only flywheel.toml `[submit] protected_paths` refuses a landing, and the hub never edits flywheel.toml.
- The file is left **uncommitted** in the main checkout; then **Run <id> with worker here**: `fw worker --task <id>` runs that task alone and exits with its outcome (the dialog lists what stays queued).
- **Fan out this task** (Flywheel panel): prompt from the task's goal and context; graders = its command graders plus its **held-out** graders, added server-side. A task with no command grader is refused; an unreadable held-out registry fails closed.
- Held-out graders never reach an agent through the hub: a prompt naming one is refused, and Ask to fix says only that a held-out check failed (and is not offered when a pinned grader script changed: that one is yours to restore). Not a sandbox (agents run as you).
- Promoting a task-seeded fan-out offers **Run <id> with worker here** (the task is already durable); a new task needs `--task-id`.

## Flywheel panel

**Postgres stores read the DSN from the hub's own environment** (`FLYWHEEL_PG_DSN`, else `DATABASE_URL`): start the hub from a shell that exports it.

Workspace folders with a `flywheel.toml` or `.flywheel/` get the verified loop beside the terminals. Read-only; `--no-flywheel` turns it off.

- Shows active phases and tasks (`pending | running | verified | failed | parked | awaiting | blocked`, from `fw status --json`), live runs, the 12 newest runs with grader receipts, worker and autopilot liveness, claims, and alerts.
- Alerts join the sidebar's **Needs you**; **Mark seen** drops them in this browser until they change.
- Open it from the folder chip's `verified/total` badge, the `flywheel` header pill (`Alt F`), or the palette.
- **Start worker here**: a hub shell running `fw worker` (the whole queue) in a split below the focused pane; a task row's **run** starts `fw worker --task <id>` (asks before adding unmet prerequisites). A second is refused (409) unless confirmed.
- **Output** on a run: its `fw show` summary and redacted agent transcript, followed live.
- A linked worktree reads its own store (`Linked worktree of <main>`). Tracker sources (`github*`) are not listed.
- Each adopted folder has one watcher (stat every 2 s, re-read on change or every 15 s). A missing SQLite store is never created.
- Seam: `flywheel-hub` defines `flywheel::FlywheelProvider`; the `flywheel` crate implements it (`hub_flywheel.rs`), so the hub never depends on the loop's crates.

## Prompt queue

Per agent session, a FIFO of prompts the hub types in itself: "when it's done, then do X". Kept by the hub owning the session (`state.json`); `--machine` and the UI reach another machine's through the proxy.

```bash
fw hub queue add api "run the tests and fix what fails"
fw hub queue add api --when idle "now write the changelog entry"
fw hub queue ls api
```

- Add from Compose (**Queue** beside Send), the drawer (header `queue` pill, `Alt Q`; a sheet on phones), the palette, or `fw hub queue add`.
- **Triggers** (`--when`): `done` (default): at rest (turn finished, or waiting for its first prompt); `idle`: at rest and a client viewed the finished turn; `blocked-cleared`: once at the front, the agent blocked, the block was answered, and that turn finished.
- **Never** while `working`, `blocked`, or `unknown`; within 1.5 s of the rest starting; within `queue.typing_quiet` of anyone typing (terminal, `attach`, `send`, Compose, the hub's own pastes; not focus reports); while a fan-out grading, a handoff, or a review action (commit, push, PR, rebase, merge, ask for a review) holds it, or the drawer's editor has the item open.
- **Unsent drafts** hold it however long: text typed into the agent's input line and not sent (send it, or clear it: Ctrl-U, Backspace, Esc Esc). Keys answering a blocked prompt do not count. No other paste lands between a delivery's text and its Enter.
- **One at a time**: the next waits for a rest that began after the last delivery. Delivery = bracketed paste, then Enter. The newest 20 delivered stay, with `delivered_at`.
- **Exactly once**: `sending` is saved before the paste, `delivered` after. A hub that died between settles it on start: a user message that is the text (whitespace aside) in the transcript -> `delivered` (`reconciled`), else pending again. `fw hub upgrade` lets a delivery under way finish and starts none until the new hub took over, and carries typing times and drafts over.
- **Stalls** stop the queue until resumed (`queue resume`, the drawer, Needs you) or relaunched: the agent exited, was not restored or came back a shell, the paste was refused, or a delivered prompt started no turn within 120 s. Removing a session drops its queue (`queue.stalled`).
- **Order**: drag or the up/down buttons; `queue move`. Pending items can be edited or removed; one being sent cannot. 50 pending per session, 64 KiB per prompt.
- Events: `queue.delivered`, `queue.stalled` ([hub-api: Plugin events](api.md#plugin-events)).

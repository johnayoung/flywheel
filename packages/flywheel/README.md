# flywheel

The top-of-stack product shell. This is the package an adopter installs: it
ships the two console scripts `flywheel` and `fw` (byte-identical entries into
the same `flywheel._cli:main`), the operator console, and the agent + container
backends, so the loop runs turnkey from a `flywheel.toml` alone.

The dist is `flywheel`; the import module is `flywheel`.

## Install

```bash
uv add flywheel
```

Installing `flywheel` pulls in `flywheel-core[claude]` (the Claude agent SDK),
`flywheel-orchestrator`, `flywheel-worktree`, and `flywheel-container`
(`pyproject.toml:11-21`) — the agent loop and the container sandbox backend
(`[sandbox] backend = "container"`) both work out of the box, no extras to opt
into.

## Quickstart

```bash
flywheel init                  # scaffold .flywheel/ + flywheel.toml (git repo required)
# authenticate the agent: export ANTHROPIC_API_KEY=... (or claude login)
flywheel worker --once         # one drain cycle over .flywheel/tasks/active/
flywheel                       # open the operator console
```

Drop one JSON task per file into `.flywheel/tasks/active/<phase>/` (or author
them with the `/fw-spec` and `/fw-plan` Claude Code skills `init` installs).

## What it owns

- **The verb router** — a thin dispatcher (`src/flywheel/_cli.py`) that forwards
  each operator verb to its pre-existing implementation in core / orchestrator /
  worktree, byte-for-byte. Verbs: `init`, `status` (+ `--rollup`), `live`,
  `history`, `show`, `archive`, `recover`, `recheck-blocked`, `validate`,
  `say`, `interrupt`, `approve`, `reject`, `worker [--once]`,
  `autopilot [--once]`, `audit`. Headless drain is `flywheel worker --once`;
  `run` / `next` / bare `orchestrate` are deliberately not exposed.
- **The interactive operator console** (Textual TUI) — bare `flywheel` (on a
  TTY) opens a dashboard of active runs, a per-run session screen, a
  finished-run history screen, and a persistent input bar with slash commands
  (`/worker start|stop`, `/autopilot start|stop`, `/approve`, `/reject`,
  `/interrupt`, …). It supervises a worker and the autopilot daemon as child
  processes and detaches them (never kills them) when the console exits.
- **`--json` snapshot mode** — `flywheel --json` (or any non-TTY stdout) prints
  one machine-readable `DashboardSnapshot` frame, no ANSI, and exits.

## Dependency arrow

`flywheel` sits at the top of the stack and depends downward on
[`flywheel-orchestrator`](../flywheel-orchestrator),
[`flywheel-worktree`](../flywheel-worktree),
[`flywheel-container`](../flywheel-container), and
[`flywheel-core`](../flywheel-core). Core never depends upward; cross-task
concepts (the DAG, claims, scheduling) live in the orchestrator, never in core.

See [../../docs/cli.md](../../docs/cli.md) for the full verb and console
reference, and [../../docs/README.md](../../docs/README.md) for the
documentation index.

# Flywheel

Production-grade orchestration loop for AI coding agents.

## Status: pre-release (WIP)

Flywheel is under active development. Public APIs, task and lifecycle
schemas, CLI flags, and on-disk layouts may change without notice between
commits. Pin to a specific commit if you depend on this code, and expect
breaking changes until a tagged 1.0 release.

## Quickstart

All paths and commands below are relative to the repo root.

**1. Confirm Python 3.13.** Flywheel requires Python 3.13 or newer; check
with `python3 --version` before continuing.

**2. Install [uv](https://docs.astral.sh/uv/getting-started/installation/),
then clone and install dependencies:**

```bash
git clone https://github.com/johnayoung/flywheel
cd flywheel
uv sync
```

**3. Write a minimal `task.json` at the repo root:**

```json
{
  "id": "readme-quickstart",
  "goal": "Print the string 'hello flywheel' to stdout.",
  "graders": [
    {
      "type": "command",
      "name": "smoke",
      "run": "echo hello flywheel | grep -q 'hello flywheel'"
    }
  ]
}
```

**4. Run the task through the workflow CLI:**

```bash
uv run python -m flywheel.workflow run task.json
```

The command exits `0` when the lifecycle reaches `done` and non-zero
otherwise. Lifecycle state is persisted to `.workflow/flywheel.sqlite`.

## Documentation

- [docs/vision.md](docs/vision.md) -- what flywheel is, and what it is not.
- [docs/task-schema.md](docs/task-schema.md) -- `Task`, `Grader`, and `Context` shapes plus validation rules.
- [docs/task-lifecycle.md](docs/task-lifecycle.md) -- lifecycle states, attempts, and transition rules.
- [docs/loop.md](docs/loop.md) -- iteration envelope (`LOOP_STATUS`) and harness behavior.
- [docs/strategy.md](docs/strategy.md) -- submission strategy (currently deferred).

## Contributing

Issues and pull requests are welcome at
<https://github.com/johnayoung/flywheel>. Read
[CONTRIBUTING.md](CONTRIBUTING.md) first for dev setup, test commands, and
the commit-message convention.

## License

License: TBD. No `LICENSE` file ships with this repository yet -- do not
assume any license is granted for reuse or redistribution.

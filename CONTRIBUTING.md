# Contributing to Flywheel

Flywheel is pre-release. Public APIs and on-disk layouts may shift between
commits; coordinate on the [project tracker](https://github.com/johnayoung/flywheel/issues)
before starting non-trivial work.

For project context and scope, read [docs/vision.md](docs/vision.md) before
opening a pull request.

## Dev setup

Python 3.13 is required. [uv](https://docs.astral.sh/uv/getting-started/installation/)
is the package manager and task runner -- do not invoke `pip` or `python`
directly.

```bash
git clone https://github.com/johnayoung/flywheel
cd flywheel
uv sync
```

## Running tests

```bash
uv run pytest                        # full suite
uv run pytest tests/test_task.py     # one file
uv run pytest -x -vv                 # stop on first failure, verbose
```

## Commit style

Commits use Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`,
`refactor:`, `test:`) -- match the existing `git log` history. Keep
subjects under 72 characters and write the body in the imperative mood.

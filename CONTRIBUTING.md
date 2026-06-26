# Contributing to Flywheel

Flywheel is pre-release. Public APIs and on-disk layouts may shift between
commits; coordinate on the [project tracker](https://github.com/johnayoung/flywheel/issues)
before starting non-trivial work.

For project context and scope, read [docs/vision.md](docs/vision.md) before
opening a pull request. Feature work follows the spec-driven pipeline in
[docs/workflow.md](docs/workflow.md), installable into any repo as the
`/fw-*` skills via `flywheel init --skills`.

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

Each package keeps its own `tests/` under `packages/<pkg>/tests/`.

Run the full suite through `scripts/run_silent.sh`, a context-efficient
backpressure wrapper: it suppresses output on success (one `ok` line) and dumps
the full output only on failure, so a green run stays quiet.

```bash
scripts/run_silent.sh "tests" uv run pytest               # full suite, quiet on success
uv run pytest packages/flywheel-core/tests/test_task.py   # one file (iterating)
uv run pytest -k transcript                                # one keyword (iterating)
uv run pytest -x -vv                                       # stop on first failure, verbose (iterating)
```

Use the bare `uv run pytest -k ...` forms while iterating on a single test; set
`RUN_SILENT_VERBOSE=1` to stream output live.

## Lint and types

CI gates every change on ruff and pyright in addition to the test suite. Run
all three before opening a PR with `scripts/check.sh`, which runs them in CI
order (ruff -> pyright -> pytest), each through the backpressure wrapper:

```bash
scripts/check.sh
```

A clean tree prints three `ok` lines; the first failing gate dumps only its own
output and stops. Treat pyright errors as build failures; the test suite does
not surface type errors.

## Commit style

Commits use Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`,
`refactor:`, `test:`) -- match the existing `git log` history. Keep
subjects under 72 characters and write the body in the imperative mood.

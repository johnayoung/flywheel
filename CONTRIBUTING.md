# Contributing to Flywheel

Flywheel is pre-release. Public APIs and on-disk layouts may shift between
commits; coordinate on the [project tracker](https://github.com/johnayoung/flywheel/issues)
before starting non-trivial work.

For project context and scope, read [docs/vision.md](docs/vision.md) before
opening a pull request. Feature work follows the spec-driven pipeline in
[docs/workflow.md](docs/workflow.md), installable into any repo as the
`/fw-*` skills via `flywheel init --skills`.

## Dev setup

Stable Rust (`rust-version` 1.91, edition 2024) via [rustup](https://rustup.rs/).

```bash
git clone https://github.com/johnayoung/flywheel
cd flywheel
cargo build                          # all six crates under crates/
cargo install --path crates/flywheel # optional: put flywheel + fw on PATH
```

## Running tests

Unit tests live in-module (`#[cfg(test)]`); integration tests in
`crates/<name>/tests/`. Docker-backed tests (Postgres, container backend) skip
themselves when no Docker daemon is reachable.

```bash
cargo test -p flywheel-core --test module_purity   # one integration-test file (iterating)
cargo test -p flywheel-core harness                # one crate, one name filter (iterating)
cargo test -p flywheel-core -- --nocapture         # show test output (iterating)
```

## The gate

CI gates every change on rustfmt, clippy (`-D warnings`), and the test suite.
Run all three before opening a PR:

```bash
scripts/check.sh
```

Each gate runs through `scripts/run_silent.sh`, a context-efficient
backpressure wrapper: a clean tree prints one `ok` line per gate; the first
failing gate dumps only its own output and stops. Set `RUN_SILENT_VERBOSE=1`
to stream output live. Treat clippy warnings as build failures.

## Commit style

Commits use Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`,
`refactor:`, `test:`) -- match the existing `git log` history. Keep
subjects under 72 characters and write the body in the imperative mood.

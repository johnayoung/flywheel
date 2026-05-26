# LKG (Last Known Good)

Flywheel dogfoods itself: `.workflow/task-worker.sh` runs flywheel against
tasks that may edit `src/flywheel/`. To stop a mid-task change from breaking
the next iteration, the worker does **not** execute the live tree. It
executes a pinned snapshot at `.workflow/lkg/`.

## The loop

1. **Edit live.** Agents and humans modify `src/flywheel/` as usual.
2. **Test.** `uv run pytest` in the live tree.
3. **Promote.** `.workflow/promote-lkg.sh` copies live -> `.workflow/lkg/`
   (refuses if tests fail) and writes the source commit SHA to
   `.workflow/lkg/SOURCE_SHA`.
4. **Next phase uses the new LKG.** The worker invokes flywheel via
   `uv run --project .workflow/lkg`, so subsequent tasks run against the
   freshly promoted code.

## Files

- `.workflow/lkg/src/flywheel/` — snapshot of the package.
- `.workflow/lkg/pyproject.toml`, `uv.lock`, `README.md` — what `uv run` needs
  to resolve the pinned environment.
- `.workflow/lkg/SOURCE_SHA` — commit SHA at promotion time
  (suffixed `-dirty` if the working tree had uncommitted changes).
- `.workflow/lkg/` is gitignored. It is local state, like the SQLite store.

## Bootstrap

If `.workflow/lkg/SOURCE_SHA` is missing on worker startup, the worker calls
`promote-lkg.sh --bootstrap` (skips the test gate) so existing checkouts
keep working without a manual step.

## Escape hatches

- `promote-lkg.sh --force` — promote without running tests. Use when the
  test failure is unrelated to flywheel itself.
- Delete `.workflow/lkg/` to force a fresh bootstrap on next worker start.

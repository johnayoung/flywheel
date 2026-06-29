# Feature: Suppress the agent co-author git trailer

## Outcome
Commits authored by a flywheel-driven agent carry no AI-attribution trailer.
The `Co-Authored-By: Claude ...` / `Generated with Claude Code` lines that
Claude Code adds by default never reach the operator's history, so flywheel
honors the common commit-hygiene rule that machine commits are attributed to the
worker identity alone.

## Background
The task agent commits inside its worktree (the commit-before-done discipline).
Claude Code's default `includeCoAuthoredBy = true` appends a `Co-Authored-By:
Claude` trailer to those commits. flywheel never suppressed it, so on
2026-06-29 a landed autopilot commit in a consumer repo carried
`Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>` — unwanted, and a
violation of the operator's stated no-AI-coauthor rule. The trailer recurs on
every agent commit.

The agent SDK exposes `ClaudeAgentOptions.settings`, forwarded to the CLI
`--settings` flag as the highest-priority "flag settings" layer (overrides
user/project settings). Passing `{"includeCoAuthoredBy": false}` there disables
the trailer at the source.

## Scope
### In scope
- `build_agent_options` (`flywheel_core.workflow`) always sets
  `settings = '{"includeCoAuthoredBy": false}'` on the task-agent
  `ClaudeAgentOptions`. This is the single tested home for task-agent option
  construction, so every driven agent (and the held-out oracle, which builds
  options the same way) inherits the suppression.

### Out of scope
- Rewriting already-committed history (stripping the trailer at land time):
  prevention at the source is simpler and tree-preserving. Existing commits that
  already carry the trailer are not rewritten.
- A config knob to re-enable the trailer: default-off matches flywheel's product
  posture (machine commits authored by the worker identity); a knob can be added
  if a real need surfaces.

### Must not regress
- The other `build_agent_options` primitives stay omit-on-unset (00037 SC-1);
  `settings` is the one deliberate always-on field.
- The exec/sandbox path still works: when `sandbox` is also set, the SDK merges
  the inline settings JSON with the sandbox settings object (verified in the SDK
  `_build_settings_value`), so both layers apply.

## Success Criteria
1. The task-agent options carry an inline `--settings` layer disabling the
   trailer. [command]
   verify: `build_agent_options(...).settings` parses to a JSON object with
   `includeCoAuthoredBy is False`.
2. The existing fast-primitive construction test stays green (no other field
   changed). [command]

## Decisions Log
### D-1: Suppress at the source via inline settings, not by rewriting commits  (Status: Accepted)
- Context: the trailer is added by the agent's own commit; the alternatives are
  prevention (settings) or cleanup (rewrite base..branch messages at land).
  | Decision: set `includeCoAuthoredBy: false` via the highest-priority inline
  `--settings` layer on every task agent. Rejected: land-time history rewrite
  (mutates SHAs, interacts with the rebase/reverify path, more surface for a
  result that prevention achieves cleanly).

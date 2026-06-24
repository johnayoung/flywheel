# flywheel-container

Container (Docker) sandbox backend for `flywheel-orchestrator` — increment G of
the sandbox-as-deploy model ([`.flywheel/specs/00044`](../../.flywheel/specs/00044-container-sandbox-backend.md)).

Runs the task agent **inside a Docker container** against a **bind-mounted**
worktree: the agent's own CLI runs in headless stream-json mode via
`docker exec`, writing through the mount to the host filesystem, so the
orchestrator's existing host-side merge/park landing path is unchanged.

**SDK-free.** The agent runs as its CLI inside the image, so this package shells
out to `docker` and never imports `claude-agent-sdk`. It is a sibling consumer
of the orchestrator (depends on `flywheel-orchestrator`), like
`flywheel-worktree`.

This module (G3) provides the Docker lifecycle primitives: argv builders,
`start_container` / `exec_in_container` (line-streamed, bounded tail) /
`remove_container`, image pre-flight (existence + UID alignment), and a
best-effort cleanup registry. The `ContainerSubmitStrategy`, the stream-json →
`IterationResult` adapter, and `[sandbox.network]` enforcement land in later
sub-increments (G4–G6).

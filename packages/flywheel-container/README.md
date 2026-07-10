# flywheel-container

Container (Docker) sandbox backend for [`flywheel-orchestrator`](../flywheel-orchestrator) — increment G of
the sandbox-as-deploy model ([`.flywheel/specs/00044`](../../.flywheel/specs/00044-container-sandbox-backend.md)).

Runs the task agent **inside a Docker container** against a **bind-mounted**
worktree: the agent's own CLI runs in headless stream-json mode via
`docker exec`, writing through the mount to the host filesystem, so the
orchestrator's existing host-side merge/park landing path is unchanged.

**SDK-free.** The agent runs as its CLI inside the image, so this package shells
out to `docker` and never imports `claude-agent-sdk`. It is a sibling consumer
of the orchestrator (depends on `flywheel-orchestrator` and `flywheel-agents`),
like [`flywheel-worktree`](../flywheel-worktree). The invocation path rides the
flywheel-agents claude-code CLI transport under `DockerExecHost`
([`docs/agent-harness.md`](../../docs/agent-harness.md) section 15.4).

## Public surface

| Symbol | Module | Role |
| --- | --- | --- |
| `start_container` / `exec_in_container` / `remove_container` | `_docker` | Docker lifecycle primitives over the CLI (line-streamed, bounded tail) |
| `build_run_argv` / `build_exec_argv` | `_docker` | Pure argv builders (unit-testable without a daemon) |
| `check_image_uid` / `image_exists` / `ensure_internal_network` | `_docker` | Image pre-flight + internal-network setup |
| `register_container_cleanup` / `force_remove_container_sync` | `_docker` | Crash-safety registry (`atexit` `docker rm -f`) |
| `ClaudeAuth` | `_auth` | Validated auth passthrough (oauth / session / api_key) |
| `ContainerRuntime` | `_submit` | Injectable seam over the Docker primitives (fakeable in tests) |
| `ContainerSubmitStrategy` | `_submit` | The `SubmitStrategy` that composes an inner landing strategy |
| `resolve_network` / `ResolvedNetwork` | `_network` | Network-policy resolver (allow / deny / internal + egress-proxy) |
| `build_container_strategy` / `resolve_auth` | `_config` | Policy → strategy/auth bridge for the worker |

## How it composes

`ContainerSubmitStrategy` (`_submit.py:95`) **wraps an inner landing strategy**
(e.g. the git-worktree merge submitter) — it does not replace landing. The inner
strategy provisions the worktree and lands host-side; this layer bind-mounts that
worktree into the container at `DEFAULT_WORKDIR`, starts it (`docker run -d`,
`--user uid:gid`), runs the agent CLI via `docker exec`, and tears the container
down. `submit` delegates to `inner.submit` unchanged (landing is backend-agnostic).

The agent runs as `claude --print --verbose --output-format stream-json` with the
prompt on stdin (`-p -`, dodging `ARG_MAX`), executed as direct argv (no `sh -c`)
by `flywheel_agents.DockerExecHost`; `--dangerously-skip-permissions` defaults
on, since a headless `--print` run has no TTY to approve tool use. The JSONL
stdout is normalized by the flywheel-agents claude-code adapter and folded into
an `IterationResult` by `flywheel_core.agents_invoke.make_agents_invoke` — every
normalized event reaches `InvocationRequest.on_message`, so the harness hang
watchdog and per-message telemetry are live on this path.

**Bind-mount gotcha (pre-flight).** `check_image_uid` (`_docker.py:181`) requires
the image's baked numeric `USER` UID to equal the host worktree owner's, else
`DockerError` with a build-arg fix hint — a UID mismatch makes the agent unable
to write through the mount. Build the image with `--build-arg AGENT_UID=$(id -u)`.

## Auth

Use `ClaudeAuth` (`_auth.py`) instead of hand-building the container env. **Prefer
a Claude Pro/Max subscription over an API key.** Three modes (use the factories,
never construct directly):

| Factory | Mechanism | Subscription |
| --- | --- | --- |
| `ClaudeAuth.oauth_token(token)` | `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`; no on-disk state, no refresh — the recommended container path | yes |
| `ClaudeAuth.session(claude_dir)` | bind-mounts host `~/.claude` read-write (UID-matched) so OAuth refresh persists | yes |
| `ClaudeAuth.api_key(key)` | raw `ANTHROPIC_API_KEY` | no |

**Guard.** `ANTHROPIC_API_KEY` wins the CLI auth order, so a subscription mode
refuses to coexist with one in the container env (`resolve`, `_auth.py:76`,
raises `ValueError`) — otherwise the key would silently shadow the session.

## Network policy

`resolve_network` (`_network.py:42`) maps `[sandbox.network]` to a Docker
`--network` decision. **The orchestrator owns attachment; the operator owns the
proxy.**

| `policy` | `allow_hosts` | Result |
| --- | --- | --- |
| `allow` (default) | — | default bridge, full egress (byte-identical to today) |
| `deny` | none | a `--internal` network — no gateway, no egress (real teeth) |
| `deny` | set | requires an operator-provisioned `egress_network` proxy; without one this is a `ValueError`, never a silent full-egress fallback |

A per-host iptables/netns firewall is deliberately not built. flywheel provides
the attachment point; the allowlisting proxy network is the operator's.

## Limitations

Load-bearing operator knowledge, documented not silently dropped:

1. **Policy `[sandbox.env]` does not reach the in-container agent.** `agent_env`
   threads into the host SDK options, which the container path bypasses; only
   `ContainerSubmitStrategy(env=...)` reaches the container. Setup and command
   graders also run host-side against the bind-mounted worktree.

(The historical "no container hang-watchdog" limitation is closed: normalized
events feed the harness observer, and `exec_timeout` remains the hard wall-clock
ceiling — expiry folds into a structured `IterationResult.failure` with
`error_type = "timeout"`; a nonzero agent exit folds into `"agent_exit"`.)

## Wiring

Operators do not call this package directly — set `[sandbox] backend = "container"`
and the worker lazy-imports it via `build_container_strategy`. See
[../../docs/container-backend.md](../../docs/container-backend.md) for the operator
quickstart (build the image, `claude setup-token`, `flywheel worker`) and
[../../docs/sandbox.md](../../docs/sandbox.md) for the full `[sandbox.*]` config
reference. The image template lives at `examples/container/`.

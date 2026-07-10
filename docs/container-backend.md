# Container execution backend

With `[sandbox] backend = "container"`, the agent's own CLI runs **inside a Docker container** — headless `claude --print --output-format stream-json`, prompt on stdin — against a **bind-mounted worktree**. Because the worktree lives on the host, the agent's edits are already on the host when the run finishes, so the merge/park landing path is unchanged: landing still runs host-side, backend-agnostic.

Status: SHIPPED and live-verified (spec 00044 increment G, config switch 00045). See [sandbox.md](sandbox.md) for the full `[sandbox.*]` model and [strategy.md](strategy.md) for the `SandboxHandle` seam this backend hangs on.

**The `flywheel-container` package is SDK-free.** Importing `flywheel_container` never needs `claude-agent-sdk` — the agent runs as its own CLI in the image; the package only shells out to `docker` (`flywheel_container/__init__.py`). The invocation itself rides the flywheel-agents claude-code CLI transport: the agent command is executed as direct argv via `docker exec` (`flywheel_agents.DockerExecHost`, no `sh -c`), including the same `--settings '{"includeCoAuthoredBy": false}'` layer the host SDK path injects, and its stream-json stdout is normalized into events and folded into the `IterationResult` by `flywheel_core.agents_invoke.make_agents_invoke` ([agent-harness.md](agent-harness.md) section 15.4). The host worker lazy-imports it only when `backend="container"` (`flywheel_worktree.worker.maybe_wrap_for_backend`, `worker.py:2166`); a missing package raises a clear install hint.

`ContainerSubmitStrategy` (`_submit.py:95`) does not replace landing — it **composes an inner landing strategy** (the git-worktree merge/PR submitter). The inner strategy provisions and lands the worktree host-side; this layer starts the container, runs the agent CLI in it, and tears the container down.

## Quickstart

No driver script is needed. Once the image exists and `[sandbox] backend = "container"` is set, `flywheel worker` is backend-aware.

1. **Build the agent image.** Use `examples/container/Dockerfile`, matching your host uid/gid:

   ```bash
   docker build -t flywheel-agent:latest \
     --build-arg AGENT_UID=$(id -u) --build-arg AGENT_GID=$(id -g) \
     examples/container/
   ```

   The image contract (`examples/container/Dockerfile`):
   - **The baked `USER` UID must equal the host uid.** Bind-mounted files must come back host-owned. The strategy pre-flights this (`check_image_uid`, `_docker.py:181`); a mismatch fails fast with a `--build-arg AGENT_UID=...` hint. This is the #1 bind-mount gotcha.
   - **`ENTRYPOINT ["sleep", "infinity"]`** — the container stays alive so the orchestrator can `docker exec` the agent CLI into it.
   - **The `claude` CLI must be on PATH** (the Dockerfile installs `@anthropic-ai/claude-code` globally as root before dropping privileges).
   - For tasks whose graders run project tooling, add Python 3.13 + uv + project deps to the image. The bundled `run_e2e.py` uses a trivial `test -f` grader, so the minimal image suffices for it.

2. **Authenticate (subscription-first).** Mint a long-lived subscription token and export it:

   ```bash
   claude setup-token
   export CLAUDE_CODE_OAUTH_TOKEN=...
   ```

3. **Configure the backend** in `flywheel.toml`:

   ```toml
   [sandbox]
   backend = "container"

   [sandbox.container]
   image = "flywheel-agent:latest"
   auth  = "oauth"
   ```

4. **Run the worker:**

   ```bash
   uv run flywheel worker
   ```

See [cli.md](cli.md) for `flywheel worker` / `fw worker`.

## Auth modes

`[sandbox.container] auth` selects how the in-container CLI authenticates (`_auth.py`, resolved by `resolve_auth`, `_config.py:23`). The token is read from a named env var **at run time** and never written to the config file.

| Mode | How | Subscription | Notes |
| --- | --- | --- | --- |
| `oauth` | `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) | yes | **Recommended.** Long-lived token via env; no on-disk state, no refresh. |
| `session` | bind-mount `~/.claude` read-write | yes | Reuses a prior `claude /login`; mounted RW so OAuth refresh persists. Container user must be UID-matched to the host owner of the dir. |
| `api_key` | raw `ANTHROPIC_API_KEY` | no | Not a subscription. |
| `none` | nothing injected | — | Operator pre-authenticated the image or passes env directly. |

`[sandbox.container] auth_env` overrides the env var name for `oauth`/`api_key`. An unset var is a clear `ValueError`, never a silent unauthenticated run (`_config.py:43`).

**Security: a subscription mode refuses to coexist with `ANTHROPIC_API_KEY` in the container env.** `ANTHROPIC_API_KEY` takes precedence over the subscription token in the CLI's auth order, so the key would silently win and the session would never be used. `ClaudeAuth.resolve` raises `ValueError` if a subscription mode (`oauth`/`session`) finds the key in the container env (`_auth.py:84`). Remove the key to authenticate with the subscription.

## Network policy

`[sandbox.network]` is advisory under the worktree backend (enforces nothing) but has **real teeth under the container backend** — it drives the container's `--network` (`resolve_network`, `_network.py:42`). The honest split: **the orchestrator owns attachment, the operator owns the proxy.**

| `policy` | `allow_hosts` | `egress_network` | Result |
| --- | --- | --- | --- |
| `allow` (default) | — | — | Default bridge, full egress (byte-identical to no sandbox). |
| `deny` | empty | — | A Docker `--internal` network: no gateway, no egress. Verifiable — `curl` from inside fails. |
| `deny` | set | required | Container is attached to the operator-provisioned egress-proxy network named by `egress_network`. |

**There is no silent full-egress fallback.** `policy = "deny"` with `allow_hosts` but no `egress_network` is a `ValueError` (`_network.py:57`). flywheel provides the attachment point; the allowlisting proxy is the operator's to run. A bespoke per-host iptables/netns firewall is deliberately not built.

```toml
[sandbox.network]
policy = "deny"
# no allow_hosts        -> --internal network, fully isolated
# allow_hosts + egress_network -> attached to your proxy net
```

## exec_timeout watchdog

`[sandbox.container] exec_timeout` (default `1800` seconds) is the hard wall-clock ceiling for the **whole in-container run**: it becomes the flywheel-agents run ceiling (`RunRequest.timeout_seconds`), so on expiry the runtime kills the `docker exec` client and the run surfaces as a structured `IterationResult.failure` with `error_type = "timeout"` (no exception; a nonzero agent exit surfaces as `error_type = "agent_exit"`). `0` means unbounded (not recommended). `build_container_strategy` maps `0` to `None` (`_config.py`).

The harness hang-watchdog is also live on this path: every stream-json line the agent emits is normalized into events forwarded to `InvocationRequest.on_message`, so `hang_timeout_seconds` sees container liveness and can be used alongside `exec_timeout`.

```toml
[sandbox.container]
exec_timeout = 1800   # seconds; 0 = unbounded (not recommended)
```

## Limitations operators must know

1. **Policy-declared `[sandbox.env]` does not reach the in-container agent.** `[sandbox.env]` (`passthrough`/`set_values`) is threaded into the **host** SDK options, which the container path bypasses entirely (the agent runs in the container CLI). Only operator-set `ContainerSubmitStrategy(env=...)` reaches the container. Closing this is a follow-on, not shipped.

2. **Setup and command graders run host-side.** The agent runs in the container, but `[sandbox] setup` and command graders run in-process against the **bind-mounted worktree on the host**. Running them inside the container is the known "grade-outside-container" gap, out of scope for v1.

## Config reference

```toml
[sandbox]
backend = "container"            # "worktree" (default) | "container"

[sandbox.container]
image          = "flywheel-agent:latest"  # REQUIRED when backend = "container"
model          = "claude-sonnet-4-6"       # default: resolved agent model
auth           = "oauth"                   # "oauth" | "session" | "api_key" | "none"
auth_env       = ""                        # override the token env var name
exec_timeout   = 1800                      # seconds; 0 = unbounded (not recommended)
egress_network = ""                        # operator egress-proxy net for allow_hosts
```

`image` is required when `backend = "container"`; the policy loader rejects its absence (`PolicyError`). `model` falls back to the worker's resolved agent model. See [configuration.md](configuration.md) for how `[sandbox.*]` is parsed and the preset/override resolution order.

## The seam and the example

The backend is built on the `SandboxHandle` seam (`flywheel_orchestrator._strategy`): `prepare_sandbox` returns a handle carrying `path` (the host worktree), `env_contribution`, an `invoke_wrapper` that runs the iteration in the container, and a per-task `teardown` that disposes the container after landing. See [strategy.md](strategy.md).

- `examples/container/Dockerfile` — the agent image template (the contract above).
- `examples/container/run_e2e.py` — the live manual driver; its header documents the full prerequisite flow (build image, `claude setup-token`, export the token, run). Overridable via `FW_IMAGE` / `FW_MODEL`.

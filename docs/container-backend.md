# Container execution backend

With `[sandbox] backend = "container"`, the agent's own CLI runs **inside a Docker container** — headless `claude --print --output-format stream-json`, prompt on stdin — against a **bind-mounted worktree**. Because the worktree lives on the host, the agent's edits are already on the host when the run finishes, so the merge/park landing path is unchanged: landing still runs host-side, backend-agnostic.

Status: SHIPPED and live-verified (spec 00044 increment G, config switch 00045). See [sandbox.md](sandbox.md) for the full `[sandbox.*]` model and [strategy.md](strategy.md) for the `SandboxHandle` seam this backend hangs on.

**The `flywheel-container` crate links no vendor SDK.** The agent runs as its own CLI in the image; the crate only shells out to `docker` (`crates/flywheel-container/src/lib.rs`). The invocation rides the flywheel-agents claude-code CLI transport: the agent command is executed as direct argv via `docker exec` (`DockerExecHost`, `crates/flywheel-agents/src/hosts.rs`, no `sh -c`), including the same `--settings '{"includeCoAuthoredBy": false}'` layer the host path injects, and its stream-json stdout is normalized into events and folded into the `IterationResult` by the core `AgentsInvoker` (`crates/flywheel-core/src/agents_invoke.rs`; [agent-harness.md](agent-harness.md) section 15.4). The worker wraps its landing strategy only when `backend = "container"` (`wrap_for_backend`, `crates/flywheel-worktree/src/daemon.rs`, delegating to `maybe_wrap_for_backend`, `crates/flywheel-container/src/config.rs`).

`ContainerSubmitStrategy` (`crates/flywheel-container/src/submit.rs`) does not replace landing — it **composes an inner landing strategy** (the git-worktree merge/PR submitter). The inner strategy provisions and lands the worktree host-side; this layer starts the container, runs the agent CLI in it, and tears the container down.

## Quickstart

No driver script is needed. Once the image exists and `[sandbox] backend = "container"` is set, `flywheel worker` is backend-aware.

1. **Build the agent image.** Use `examples/container/Dockerfile`, matching your host uid/gid:

   ```bash
   docker build -t flywheel-agent:latest \
     --build-arg AGENT_UID=$(id -u) --build-arg AGENT_GID=$(id -g) \
     examples/container/
   ```

   The image contract (`examples/container/Dockerfile`):
   - **The baked `USER` UID must equal the host uid.** Bind-mounted files must come back host-owned. The strategy pre-flights this (`Docker::check_image_uid`, `crates/flywheel-container/src/docker.rs`); a mismatch fails fast with a `--build-arg AGENT_UID=...` hint. This is the #1 bind-mount gotcha.
   - **`ENTRYPOINT ["sleep", "infinity"]`** — the container stays alive so the orchestrator can `docker exec` the agent CLI into it.
   - **The `claude` CLI must be on PATH** (the Dockerfile installs `@anthropic-ai/claude-code` globally as root before dropping privileges).
   - For tasks whose agent needs project tooling (compilers, package managers), add it to the image. Command graders run host-side (see Limitations), so they use the host toolchain.

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
   flywheel worker
   ```

See [cli.md](cli.md) for `flywheel worker` / `fw worker`.

## Auth modes

`[sandbox.container] auth` selects how the in-container CLI authenticates (`ClaudeAuth`, `crates/flywheel-container/src/auth.rs`, resolved by `resolve_auth`, `crates/flywheel-container/src/config.rs`). The token is read from a named env var **at run time** and never written to the config file.

| Mode | How | Subscription | Notes |
| --- | --- | --- | --- |
| `oauth` | `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) | yes | **Recommended.** Long-lived token via env; no on-disk state, no refresh. |
| `session` | bind-mount `~/.claude` read-write | yes | Reuses a prior `claude /login`; mounted RW so OAuth refresh persists. Container user must be UID-matched to the host owner of the dir. |
| `api_key` | raw `ANTHROPIC_API_KEY` | no | Not a subscription. |
| `none` | nothing injected | — | Operator pre-authenticated the image or passes env directly. |

`[sandbox.container] auth_env` overrides the env var name for `oauth`/`api_key`. An unset or empty var is a clear `ConfigError::MissingToken`, never a silent unauthenticated run.

**Security: a subscription mode refuses to coexist with `ANTHROPIC_API_KEY` in the container env.** `ANTHROPIC_API_KEY` takes precedence over the subscription token in the CLI's auth order, so the key would silently win and the session would never be used. `ClaudeAuth::resolve` fails with `AuthError::ApiKeyConflict` if a subscription mode (`oauth`/`session`) finds the key in the container env. Remove the key to authenticate with the subscription.

## Network policy

`[sandbox.network]` is advisory under the worktree backend (enforces nothing) but has **real teeth under the container backend** — it drives the container's `--network` (`resolve_network`, `crates/flywheel-container/src/network.rs`). The honest split: **the orchestrator owns attachment, the operator owns the proxy.**

| `policy` | `allow_hosts` | `egress_network` | Result |
| --- | --- | --- | --- |
| `allow` (default) | — | — | Default bridge, full egress (byte-identical to no sandbox). |
| `deny` | empty | — | A Docker `--internal` network: no gateway, no egress. Verifiable — `curl` from inside fails. |
| `deny` | set | required | Container is attached to the operator-provisioned egress-proxy network named by `egress_network`. |

**There is no silent full-egress fallback.** `policy = "deny"` with `allow_hosts` but no `egress_network` is a `NetworkError::EgressNetworkRequired`. flywheel provides the attachment point; the allowlisting proxy is the operator's to run. A bespoke per-host iptables/netns firewall is deliberately not built.

```toml
[sandbox.network]
policy = "deny"
# no allow_hosts        -> --internal network, fully isolated
# allow_hosts + egress_network -> attached to your proxy net
```

## exec_timeout watchdog

`[sandbox.container] exec_timeout` (default `1800` seconds) is the hard wall-clock ceiling for the **whole in-container run**: it becomes the flywheel-agents run ceiling (`RunRequest.timeout_seconds`), so on expiry the runtime terminates the agent's in-container process group (SIGTERM, grace, SIGKILL -- each a `docker exec kill` against the pgid the agent wrapper recorded under `/tmp`) and then the `docker exec` client, and the run surfaces as a structured `IterationResult.failure` with `error_type = "timeout"` (no exception; a nonzero agent exit surfaces as `error_type = "agent_exit"`). `0` means unbounded (not recommended); `build_container_config` maps `0` to no ceiling.

Non-agent in-container commands (`Docker::exec_in_container`, e.g. setup hooks) use the same mechanism: the command runs under a wrapper that records its pgid in `/tmp/flywheel-exec-<uuid>.pgid` (removed on a normal exit); on `ExecOptions.timeout` the group gets SIGTERM, `kill_grace` (default 5s), then SIGKILL (which also removes the file), and only then is the client killed. SIGTERM-ignoring grandchildren do not survive.

The harness hang-watchdog is also live on this path: every stream-json line the agent emits is normalized into events forwarded to `InvocationRequest.on_message`, so `hang_timeout_seconds` sees container liveness and can be used alongside `exec_timeout`.

```toml
[sandbox.container]
exec_timeout = 1800   # seconds; 0 = unbounded (not recommended)
```

## Git metadata mounts

The agent commits its own work in the container (an uncommitted tree parks as `uncommitted-work`). A linked worktree's `.git` is a file naming host paths outside the worktree (`gitdir: <repo>/.git/worktrees/<id>`, then `commondir`), so the strategy also bind-mounts the repository's **common git dir at the path the `.git` file names** (`git_metadata_mounts`, `crates/flywheel-container/src/gitdir.rs`). Without it every in-container `git` command fails with `fatal: not a git repository`.

| Mount | Mode | Why |
|---|---|---|
| worktree -> `/home/agent/workspace` | rw | the agent's edits |
| `<repo>/.git` -> same path (relative gitdirs resolve against the workdir) | rw | a commit writes objects, the task branch ref, and the per-worktree index/HEAD |
| `<repo>/.git/config` | ro | host git reads it: a planted `core.fsmonitor`/`core.hooksPath`/filter would run on the host |
| `<repo>/.git/hooks` (created empty if missing) | ro | hooks run on the host at landing and in the operator's checkout |

**Security:** the agent can read and rewrite any ref and object of the repository (other branches, `main`) -- the same reach it has on the worktree backend, where the agent runs as the host user. It cannot change the shared repo config or hooks. Residual: the worktree's own `.git` file is agent-writable (as before this mount), so it can redirect git commands run *in that worktree*. A plain directory or a `.git` directory sandbox gets no extra mounts.

## Container lifecycle and orphan reap

Containers are started without `--rm` so a dead sandbox stays inspectable (`docker logs`, exit code) until teardown (`stop` + `rm`) or the exit-time registry removes it. Each container carries owner labels: host, pid, and (Linux) process start time and pid namespace. At worker boot, `reap_container_orphans` (on a background thread alongside the first cycle, joined before the worker exits) removes only flywheel containers whose owner is provably dead **on this host and in this pid namespace** (pid gone, or pid reused by another process); live owners, other hosts', and other namespaces' containers (sibling WSL2 distros, containerised workers) are never touched. Containers without owner labels (pre-0.6.0) are treated as orphans.

## Limitations operators must know

1. **Policy-declared `[sandbox.env]` does not reach the in-container agent.** `[sandbox.env]` (`pass`/`set`) resolves into the **host** agent environment; the policy bridge builds the container config with an empty `env` (`container_config_from_policy`), so only an operator-constructed `ContainerConfig.env` reaches the container. Likewise `[sandbox.capabilities]` does not reach it: the in-container agent configuration carries only model and permission policy (`container_agent_configuration`, `crates/flywheel-worktree/src/daemon.rs`). Closing this is a follow-on, not shipped.

2. **Setup and command graders run host-side.** The agent runs in the container, but `[sandbox] setup` and command graders run on the host against the **bind-mounted worktree**. Running them inside the container is the known "grade-outside-container" gap, out of scope for v1.

## Config reference

```toml
[sandbox]
backend = "container"            # "worktree" (default) | "container"

[sandbox.container]
image          = "flywheel-agent:latest"  # REQUIRED when backend = "container"
model          = "claude-sonnet-4-6"       # default: resolved agent model; neither set is an error
auth           = "oauth"                   # "oauth" | "session" | "api_key" | "none"
auth_env       = ""                        # override the token env var name
exec_timeout   = 1800                      # seconds; 0 = unbounded (not recommended)
egress_network = ""                        # operator egress-proxy net for allow_hosts
```

`image` is required when `backend = "container"`; the policy loader rejects its absence (`PolicyError`). `model` falls back to the worker's resolved agent model (`[agent] model` or `--model`); with neither set the worker fails with `ConfigError::MissingModel`, since the CLI is invoked with an explicit `--model`. See [configuration.md](configuration.md) for how `[sandbox.*]` is parsed and the preset/override resolution order.

## The seam and the example

The backend is built on the `SandboxHandle` seam (`crates/flywheel-orchestrator/src/strategy.rs`): `prepare_sandbox` returns a handle carrying `path` (the host worktree), `env_contribution`, an `invoke_wrapper` that runs the iteration in the container, and a per-task `teardown` that disposes the container after landing. See [strategy.md](strategy.md).

- `examples/container/Dockerfile` — the agent image template (the contract above).
- `examples/container/run_e2e.sh` — end-to-end check: drives `flywheel worker --once` with `backend = "container"` and asserts the task lands on main, file ownership is correct, and the container is removed.

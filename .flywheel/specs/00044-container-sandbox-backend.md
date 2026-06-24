# 00044 — Container sandbox backend (increment G of 00036)

Status: IMPLEMENTED + LIVE-VERIFIED (G1–G6 shipped; SC-9 confirmed end-to-end on
2026-06-24 — a real task ran agent-in-container to DONE via
`examples/container/run_e2e.py`, authenticated with a Claude subscription
session, no API key). Increment G of
[00036](00036-sandbox-deploy-model.md): a new `flywheel-container` workspace
package providing a `ContainerSubmitStrategy`
that runs the task agent **inside a Docker container** against a bind-mounted
worktree, with a `teardown()` lifecycle hook and `[sandbox.network]` enforcement
via Docker networking. The major increment; the seam it plugs into
([[00043 SandboxHandle|00043-sandbox-handle-seam.md]]) already shipped.

Validated against a real-world implementation —
[[reference_sandcastle]] (`mattpocock/sandcastle`) does exactly this for Claude
Code, Codex, Cursor, etc. Its patterns are adopted throughout; deviations are
called out.

## The insight that makes G tractable

The earlier read of G — "run the agent inside a container" — assumed
reimplementing the agent SDK's transport. It does not. The agent runs as its own
**CLI binary in headless stream-json mode** inside the container, driven by
`docker exec`:

```
claude --print --verbose --output-format stream-json --model <m> -p -   # prompt on stdin
```

The `claude` CLI is exactly what `claude_agent_sdk` wraps. So a container
`InvokeFunc` (installed via the F-seam `invoke_wrapper`) execs the CLI, streams
its JSONL stdout, and adapts it into flywheel's `IterationResult`. The only novel
flywheel-specific logic is that **stream-json → `IterationResult` adapter**, which
is a *pure, fixture-testable parser* — not an unverifiable integration.

## Package decision (resolved)

**`flywheel-container`** (00036 §7-open-4). The repo names packages by isolation
*mechanism* (`flywheel-worktree`); a worktree is itself a sandbox backend, so
`flywheel-sandbox` would over-claim the umbrella concept that already lives in
the orchestrator (`SandboxPolicy`, `[sandbox.*]`). A future microVM/cloud
backend is its own thin sibling (`flywheel-firecracker`, …), each a consumer of
the orchestrator. Dependency arrow: `flywheel-container` → `flywheel-orchestrator`
(+ `flywheel-core`), never the reverse; a sibling of `flywheel-worktree`.

**SDK-free (00036 §6).** `flywheel-container` MUST import without
`claude_agent_sdk` (it shells out to the CLI in the image; it needs no Python
SDK). Enforced by an import-purity test mirroring the core purity tests.

## Architecture (sandcastle-derived)

One spine per task run:

1. **`prepare_sandbox`** — `docker run -d` a long-lived container
   (`ENTRYPOINT ["sleep","infinity"]`) from a pre-built image, **bind-mounting
   the host worktree** at a fixed workdir. Return a `SandboxHandle` whose `path`
   is the host worktree (unchanged — landing/merge still runs host-side) and
   whose `invoke_wrapper` runs the agent in the container.
2. **run** — the harness calls the wrapped `InvokeFunc`, which `docker exec -i`s
   the agent CLI (prompt on stdin), streams JSONL stdout, and adapts it to an
   `IterationResult`.
3. **`submit`** — unchanged: the worktree is on the host, so the existing
   merge/PR landing path works as-is (the killer simplification of bind-mount).
4. **`teardown()`** — stop + `rm` the container; a sync shutdown-registry also
   `docker rm -f`s on SIGINT/SIGTERM/exit (crash safety).

## Sub-increments (ordered, each independently shippable)

G is itself a mini-program. Each lands as its own `fw-spec`/task (spec numbers
assigned when scheduled):

| # | Sub-increment | Scope | Depends |
| --- | --- | --- | --- |
| **G1** | **`teardown` seam** | Add optional `teardown: Callable[[], None] \| None` to `SandboxHandle` (per-task, closure-captured over the container — symmetric with `invoke_wrapper`, not a `SubmitStrategy` method which a shared strategy instance couldn't key to one task's container). `orchestrate` calls `handle.teardown()` best-effort after `submit` (must not raise). The worktree path (bare `Path` → `teardown=None`) is untouched — no no-op needed. Matches sandcastle's `handle.close()`. Small, self-contained, useful alone. | — |
| **G2** | **Plain-dict usage path in core** | `IterationResult` gains optional `usage: Mapping[str,int] \| None`; the harness rollup (`harness.py:3207`) reads it when set, falling back to `_build_usage_breakdown(messages)` when not. Lets a message-less (SDK-free) invoker feed token accounting + the D token ceiling. SDK-free, ~no behavior change for the SDK path. | — |
| **G3** | **Container lifecycle primitives** | `flywheel-container` package scaffold + `run -d` / `exec` (line-streamed, bounded tail) / `cp` / stop+rm, UID/GID-aligned `--user`, pre-flight image+UID check, sync shutdown-registry. Verified against the live daemon. | G1 |
| **G4** | **stream-json → IterationResult adapter** | Pure parser: JSONL lines → accumulated transcript, `session_id`, `result`, token `usage` (4 fields), `total_cost_usd`, `num_turns`; `envelope = parse_envelope(transcript)` (reuse core); `signals = InvocationSignals(...)`. Fixture-tested. | G2 |
| **G5** | **`ContainerSubmitStrategy`** | Assemble G3+G4: `prepare_sandbox` returns a `SandboxHandle(path, invoke_wrapper, env_contribution)`; `submit` reuses worktree landing; `teardown()` from G1. | G3, G4 |
| **G6** | **`[sandbox.network]` enforcement** | Map policy to Docker networking (below). | G5 |

## The stream-json adapter (G4) — the core novelty

A pure function `parse_stream_json_lines(lines) -> ContainerIteration` returning
the fields flywheel needs, then a thin `InvokeFunc` that wires it to a
(seam-injected) exec. JSONL schema (from the CLI; confirmed by sandcastle):

- `{type:"assistant", message:{content:[{type:"text",text}|{type:"tool_use",…}], usage:{input_tokens,output_tokens,cache_creation_input_tokens,cache_read_input_tokens}}}` → accumulate text; sum usage.
- `{type:"result", result, total_cost_usd, num_turns}` → final result + cost + turns.
- `{type:"system", subtype:"init", session_id}` → session id.

The adapter produces an `IterationResult{transcript, messages:(), envelope:
parse_envelope(transcript), signals: InvocationSignals(stop_reason, num_turns,
total_cost_usd, …), usage:{…}, failure}`. `messages=()` is why G2 exists.

**Known v1 limitations (documented, not silently dropped):**

- `InvocationRequest.on_message` is the harness's per-message observer over SDK
  `Message` objects. A message-less container invoker cannot call it, so live
  per-message telemetry is absent (the JSONL stream is still captured wholesale;
  envelope/signals/usage land). **Consequence:** the harness *hang watchdog*
  reads its heartbeat from `on_message` (`harness.py`), so it cannot see a
  container run's liveness — with `hang_timeout_seconds` set it would false-trip
  a healthy long run, and unset it provides no bound. **Mitigation:**
  `ContainerSubmitStrategy(exec_timeout=...)` bounds the whole `docker exec`
  (a kill-timer in `exec_in_container`, effective even when the agent holds
  stdout open and silent); operators set it instead of the host watchdog.
  Synthesizing `Message` records to feed the watchdog is a later refinement.
- `[sandbox.env]` (increment C) resolves into the run's `agent_env` and is
  threaded into the *host* SDK options — which the container path bypasses. So
  policy-declared env does not currently reach the in-container agent; only the
  operator-set `ContainerSubmitStrategy(env=...)` does. Closing this means
  threading `agent_env` to `prepare_sandbox` (via `SandboxRequest`) so the
  container is started with it — a follow-on, not in G5.

## Network model (G6) — honest teeth

Per 00036 §6, real network enforcement needs a non-worktree backend; this is it.
Map `[sandbox.network]` to Docker networking (sandcastle's division: the
orchestrator owns *attachment*, the operator owns the *proxy*):

- `policy = "allow"` (default) — no `--network` flag; default bridge; full
  egress. Byte-identical to today.
- `policy = "deny"`, empty `allow_hosts` — attach to a Docker **`--internal`**
  network (Docker internal networks have **no external connectivity**). Real,
  verifiable teeth: `curl` from inside fails.
- `policy = "deny"`, non-empty `allow_hosts` — attach to an
  **operator-provisioned egress-proxy network** (named via a provider option).
  flywheel provides the attachment point; the allowlisting proxy is the
  operator's (a bespoke per-host iptables/netns firewall is explicitly NOT built
  — it is fragile and is not what a shipped tool does).
- `allow_unix_sockets` — bind-mount the socket path(s) (`-v`). Optional/deferred.

The doc must state plainly: under `flywheel-container`, `deny` with empty
`allow_hosts` is fully enforced; `allow_hosts` is enforced *only* with an
operator egress network attached.

## Image contract

A `.flywheel/Dockerfile` template (init scaffolds it):
`FROM <base>`; install git + the agent CLI; `ARG AGENT_UID/GID` defaulted to the
host user; `USER`; `ENTRYPOINT ["sleep","infinity"]`; worktree bind-mounted at a
fixed workdir.

Agent auth is a first-class knob — `ContainerSubmitStrategy(auth=ClaudeAuth.…)`:
`oauth_token` (a `claude setup-token` subscription token → `CLAUDE_CODE_OAUTH_TOKEN`
env; the recommended container path, no API key), `session` (bind-mount the
host's `~/.claude` login, read-write for refresh), or `api_key`. Subscription
modes refuse to coexist with an `ANTHROPIC_API_KEY` in the container env (the CLI
prefers the key and would ignore the session). Never baked into the image.

## Success criteria (each → grader; tier noted)

- **SC-1 (unit)** — G4 parser maps fixture JSONL to the correct
  `{transcript, session_id, result, usage(4 fields), total_cost_usd, num_turns}`.
- **SC-2 (unit)** — the container `InvokeFunc`, given a seam-injected exec that
  replays fixture lines, returns an `IterationResult` with the right
  `envelope` (via `parse_envelope`), `signals`, and `usage`. No real Docker.
- **SC-3 (unit)** — G2: a message-less `IterationResult` with `usage` set rolls
  its tokens into the attempt (and trips the D `max_tokens` ceiling); the SDK
  message path is unchanged.
- **SC-4 (integration, live docker; skip if absent)** — lifecycle: `run -d` a
  generic image, `exec` a command (streamed), `teardown` removes it.
- **SC-5 (integration, live docker)** — bind-mount + UID alignment: a file
  written by the container user lands host-owned by the host uid.
- **SC-6 (integration, live docker)** — `policy="deny"` empty `allow_hosts`:
  egress from the container fails; on default network it succeeds.
- **SC-7 (unit)** — G1: `orchestrate` calls `strategy.teardown()` after `submit`;
  worktree teardown is a no-op; a raising teardown does not unwind the worker.
- **SC-8 (unit)** — `import flywheel_container` succeeds with `claude_agent_sdk`
  absent (SDK-free).
- **SC-9 (live — VERIFIED 2026-06-24)** — E2E: a built image with the agent CLI
  + valid auth runs a real task in-container and lands it. Confirmed via
  `examples/container/run_e2e.py` against a host Docker daemon with a Claude
  subscription session (`CLAUDE_CODE_OAUTH_TOKEN`, no API key): a fresh run
  reached `Status.DONE` with the agent's file written through the bind mount and
  the container torn down. Requires image + auth, so it stays a marked/manual
  test rather than default CI.

## Out of scope

Podman/microVM/cloud backends (own siblings); in-container session resume
(host↔sandbox JSONL transfer — sandcastle does it, flywheel can later); per-host
iptables/netns firewall (delegated to an egress proxy); `--cpus`/`--memory`/`--pids`
limit mapping (natural follow-on to `[sandbox.limits]`); synthesizing
`on_message` records for container runs.

## Anchor files

- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_strategy.py` —
  `SandboxHandle` (the seam, shipped; G1 adds `teardown`).
- `packages/flywheel-orchestrator/src/flywheel_orchestrator/_orchestrate.py` —
  call `strategy.teardown()` after `submit` in the drive path.
- `packages/flywheel-core/src/flywheel_core/harness.py` — `IterationResult`
  (+ `usage`), the rollup at ~3207; `InvocationRequest.prompt` (stdin source);
  `parse_envelope` reuse.
- `packages/flywheel-worktree/src/flywheel_worktree/worker.py` — the reference
  `SubmitStrategy` (no-op `teardown()`); the bind-mount provider mirrors its
  `prepare_sandbox`/retention shape.
- NEW `packages/flywheel-container/` — the package.

## Decision gates (resolve in each sub-increment's fw-spec)

- **G1:** `teardown` lives on the `SandboxHandle` (resolved — per-task closure,
  symmetric with `invoke_wrapper`), called after `submit`, before lease release,
  best-effort/must-not-raise (mirrors `submit`'s contract). Arg-less for now;
  status-conditional preserve (retention-aware container teardown) is a G5
  refinement that widens the callable then.
- **G2:** `usage` on `IterationResult` vs `InvocationSignals`? (Recommend
  `IterationResult` — it already carries `messages`; `usage` is the message-less
  peer, keeping `InvocationSignals` about the result/stop signals.)
- **G6:** the egress-proxy network's name/convention — provider option vs a
  `[sandbox.network]` field. (Recommend a provider option; the policy stays
  declarative.)

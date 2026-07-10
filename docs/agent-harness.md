# flywheel-agents — the multi-agent execution layer

**Status: design document.** Target architecture for the `flywheel-agents` package; not yet shipped behavior. Execution is phased through `.flywheel/specs/` (see § 16). Where this doc and shipped code disagree, the code's own spec set (`docs/README.md`) wins until the corresponding phase lands.

## 1. Objective

One consistent way to run coding-agent harnesses — Claude Code, OpenAI Codex, Gemini CLI, Qwen Code, OpenCode, Amp, and others — as the brain inside flywheel's loop. The package invokes the **real agent harnesses**, preserving their native authentication, subscription plans, tools, configuration, MCP servers, model selection, and permission systems.

It is **not**: an LLM router, a model API abstraction, an OpenAI-compatible proxy, a common re-implementation of coding-agent behavior, or a TUI screen-scraper.

```text
flywheel loop (InvokeFunc seam)
    │
    ▼
flywheel_agents runtime  ── normalized events + CompletedRun fold
    │
    ├── claude-code adapter ── SDK transport (local) / CLI stream-json (any host)
    ├── codex adapter ──────── app-server JSON-RPC
    ├── gemini adapter ─────── ACP
    ├── qwen adapter ────────── ACP
    └── other adapters ─────── best available structured interface
```

The package standardizes the **control plane**, not the agent implementation.

### Position in the workspace

New bottom-of-stack package: `packages/flywheel-agents` (import `flywheel_agents`). It imports **nothing** from any flywheel package and is publishable standalone. `flywheel-core` consumes it through an optional extra (`flywheel-core[agents]`), generalizing the existing `[claude]` extra: `import flywheel_core` must keep working with no agent SDK or CLI installed. The lazy boundary role of `flywheel_core._sdk` moves to a single bridge module (§ 15).

Named `flywheel-agents`, not `flywheel-harness`: "harness" is already the loop harness (`flywheel_core.harness`); this doc uses "agent harness" only when referring to the vendor CLIs themselves.

## 2. Requirements

### 2.1 v1 core requirements

1. Start a run and stream normalized events.
2. Run to completion and return a structured fold (`CompletedRun`) — flywheel's unit of work is one loop iteration, drained to completion.
3. Cancel a running agent; reliably terminate the full process tree.
4. Preserve every raw native event before normalization.
5. Report per-adapter capabilities honestly.
6. Probe installation and authentication status.
7. Select working directory; construct the child environment per policy.
8. Drive the same adapter under a local subprocess **or** `docker exec` (execution-host seam, § 7).
9. Run sessions concurrently; Linux, macOS, WSL2 (Windows-native later).
10. Prefer the agent's account-plan authentication wherever supported.

### 2.2 Explicit v1 non-requirements

- Feature parity across agents; prompt translation; replicating native tools; managing subscriptions; parsing interactive TUIs.
- **An HTTP/WebSocket service.** Library only. A service surface belongs to the interactivity program and wraps this runtime later.
- **Package-owned persistence.** No SQLite, no `~/.agent-harness`. The package emits through a sink protocol (§ 9); flywheel's telemetry sink and relational store are the system of record.
- **Interactive approvals.** flywheel runs unattended. Normalized approval events plus an auto-resolving policy callback are kept (§ 12); no approval UI or queue subsystem.
- **Profiles.** `flywheel.toml` is the profile layer.
- **MCP config-file writing.** Canonical read/translate only (§ 13).
- **Session resume/fork/replay.** Fresh-session-per-iteration is a deliberate flywheel design choice; `native_session_id` is captured and reported, everything else is a capability flag for later.

## 3. Design principles

1. **Native harness first.** Use the deepest structured interface available: official structured protocol or SDK > ACP > JSON-RPC > NDJSON/stream JSON > structured one-shot output > plain text. TUI automation is not a supported integration.
2. **Preserve native behavior.** Adapters never reimplement file editing, shell execution, repo indexing, model calls, compaction, or native subagents.
3. **Capability honesty.** Expose what each adapter actually implements; never imply parity. flywheel's loop guards degrade explicitly per capability, not silently.
4. **Raw events are immutable.** Every native event is retained before normalization — debugging, replay, normalizer migration, audit.
5. **Adapters translate; the runtime orchestrates.** Agent-specific code lives in adapters. Process lifecycle, event fan-out, cancellation, environment and auth policy live in the runtime.
6. **Storage-free.** Emit, don't persist. All durable state belongs to the caller.
7. **argv construction and process spawning are separate axes.** Adapters produce a `ProcessPlan`; execution hosts spawn it. This is what lets one adapter serve both the worktree and container backends.
8. **Run-to-completion is first-class.** The streaming API exists, but the supported product contract is the `CompletedRun` fold (§ 6.2).

## 4. Package structure

Flat private-module layout, matching sibling-package idiom (`flywheel_container`'s `_docker.py`/`_submit.py`):

```text
packages/flywheel-agents/
├── pyproject.toml
├── src/flywheel_agents/
│   ├── __init__.py            # the public surface (re-exports)
│   ├── adapter.py             # AgentAdapter / RunningAgent / AdapterServices
│   ├── capabilities.py
│   ├── config.py
│   ├── errors.py
│   ├── events.py              # EventType / AgentEvent / RawAgentEvent / sinks
│   ├── fold.py                # event stream -> CompletedRun
│   ├── hosts.py               # ProcessPlan / LocalHost / DockerExecHost
│   ├── models.py
│   ├── registry.py            # entry-point plugins: flywheel_agents.adapters
│   ├── runtime.py             # AgentRuntime.run()
│   └── claude_code/
│       ├── adapter.py         # transport routing (cli | sdk)
│       ├── _cli.py            # stream-json plan + normalizer (any host)
│       ├── _sdk.py            # claude-agent-sdk transport (local only)
│       ├── _common.py         # shared usage/stop normalization
│       ├── _probe.py
│       └── _faults.py         # session-limit fault classification
└── tests/                     # unit + contract suite + fake_agent.py
```

ACP / JSON-RPC transport modules, the auth-policy subsystem, and the MCP
canonical schema arrive with their phases (section 16), as do additional
adapters (`codex/`, `gemini/`, `qwen/`, ...).

## 5. Domain model

Python 3.13. Frozen dataclasses with `slots=True`, pyright strict — matching flywheel-core idiom. No Pydantic: the package sits at the bottom of the dependency graph and must not introduce a second modeling stack.

### 5.1 Agent identity

```python
@dataclass(frozen=True, slots=True)
class AgentDescriptor:
    id: str                      # "claude-code", "codex", "gemini-cli", ...
    display_name: str
    adapter_version: str
    vendor: str | None = None
    executable_names: tuple[str, ...] = ()
    capabilities: AgentCapabilities = AgentCapabilities()
```

Agent ids are open strings, never a closed enum — third-party adapters register via entry points (§ 14).

### 5.2 Capabilities

Grouped by what consumes them. The **signal fidelity** group is the loop-guard input map: each flag corresponds to a detection row in `docs/loop.md`.

```python
@dataclass(frozen=True, slots=True)
class AgentCapabilities:
    # session
    native_resume: bool = False
    native_fork: bool = False

    # control
    cancellation: bool = True
    mid_turn_interrupt: bool = False
    mid_turn_model_change: bool = False       # control-command set_model
    mid_turn_prompt_injection: bool = False   # control-command say
    approvals: bool = False

    # selection / discovery
    model_selection: bool = False
    model_discovery: bool = False
    mode_selection: bool = False
    reasoning_selection: bool = False

    # signal fidelity (loop-guard inputs)
    structured_tool_calls: bool = False       # stuck/thrash counters
    tool_result_errors: bool = False          # blocked_implicit counter
    thought_events: bool = False              # hang-watchdog liveness
    context_usage: bool = False               # context_exhausted detection
    rate_limit_events: bool = False           # rate_limited flag + ETA
    hook_events: bool = False

    # ecosystem
    subagents: bool = False                   # SDK AgentDefinition-style
    mcp_configuration: bool = False
    slash_commands: bool = False

    # auth
    account_login_detection: bool = False
    api_key_detection: bool = False
```

Capabilities describe the **adapter implementation**, not theoretical vendor functionality.

### 5.3 Configuration

```python
class PermissionPolicy(StrEnum):
    AUTO = "auto"            # flywheel's mode: translated to the agent's native bypass
    SUPERVISED = "supervised"
    PLAN = "plan"
    READ_ONLY = "read-only"


class AuthenticationPolicy(StrEnum):
    ACCOUNT_PREFERRED = "account-preferred"
    ACCOUNT_ONLY = "account-only"
    API_ALLOWED = "api-allowed"


@dataclass(frozen=True, slots=True)
class AgentConfiguration:
    agent_id: str
    model_id: str | None = None
    mode_id: str | None = None
    reasoning_id: str | None = None
    permission_policy: PermissionPolicy = PermissionPolicy.SUPERVISED
    authentication_policy: AuthenticationPolicy = AuthenticationPolicy.ACCOUNT_PREFERRED
    max_turns: int | None = None
    command_override: tuple[str, ...] | None = None
    environment: Mapping[str, str] = field(default_factory=dict)
    adapter_options: Mapping[str, object] = field(default_factory=dict)
```

`adapter_options` carries what the generic fields cannot honestly represent. For claude-code this is where today's `[sandbox.capabilities]` vocabulary lands: `skills`, `allowed_tools`, `disallowed_tools`, `setting_sources`, `mcp_servers`, `mcp_strict`, `sandbox_exec`, plus the no-coauthor settings injection.

## 6. Adapter contract

```python
class AgentAdapter(ABC):
    @property
    @abstractmethod
    def descriptor(self) -> AgentDescriptor: ...

    @abstractmethod
    async def probe(self) -> AgentProbeResult: ...

    @abstractmethod
    async def discover_options(self, context: DiscoveryContext) -> AgentOptions: ...

    @abstractmethod
    async def start(
        self, request: RunRequest, services: AdapterServices
    ) -> RunningAgent: ...

    def classify_fault(self, evidence: FaultEvidence) -> AgentFault | None:
        return None
```

`classify_fault` generalizes what `flywheel_core.faults` hardcodes today (the `"Claude AI usage limit reached|<epoch>"` regexes): adapters own recognition of vendor-specific fault strings and map them to a normalized taxonomy (`session_limit` with `resets_at`, `auth_expired`, `quota`, `infra`).

### 6.1 Running agent

```python
class RunningAgent(ABC):
    @property
    @abstractmethod
    def native_session_id(self) -> str | None: ...

    @abstractmethod
    def events(self) -> AsyncIterator[AgentEvent]: ...

    @abstractmethod
    async def cancel(self) -> None: ...

    @abstractmethod
    async def wait(self) -> AgentExit: ...
```

Mid-turn control (`interrupt`, `set_model`, `inject_prompt`) is exposed through optional mixin methods gated by the corresponding capability; calling one on an adapter without the capability raises `UnsupportedCapabilityError`.

### 6.2 The fold: `CompletedRun`

The supported contract for flywheel. The runtime drains the event stream and folds it:

```python
result: CompletedRun = await runtime.run(request, sink=sink)


@dataclass(frozen=True, slots=True)
class CompletedRun:
    final_text: str                       # concatenated assistant text
    stop: StopInfo                        # normalized reason + native string + pending_tool_use
    usage: Mapping[str, int] | None       # input/output/cache token counters
    total_cost_usd: float | None
    num_turns: int | None
    native_session_id: str | None
    tool_interactions: tuple[ToolInteraction, ...]
    permission_denials: tuple[PermissionDenial, ...]
    rate_limit_events: tuple[RateLimitInfo, ...]
    fault: AgentFault | None
    failure: RunFailure | None            # transport/process failure, not agent output
    exit: AgentExit
```

```python
class StopReason(StrEnum):
    COMPLETED = "completed"
    MAX_TOKENS = "max-tokens"
    MAX_TURNS = "max-turns"
    CANCELLED = "cancelled"
    ERROR = "error"
    UNKNOWN = "unknown"
```

Native stop strings are preserved alongside the normalized enum. This fold replaces the two divergent implementations that exist today: `flywheel_core.invoker.invoke_iteration`'s SDK-message dispatch and `flywheel_container._stream.parse_stream_json`'s dict dispatch.

## 7. Execution hosts

The seam the original spec deferred and flywheel needs on day one. Adapters build a `ProcessPlan`; hosts spawn it.

```python
@dataclass(frozen=True, slots=True, kw_only=True)
class ProcessPlan:
    argv: tuple[str, ...]
    cwd: Path | None = None
    environment: Mapping[str, str] = field(default_factory=dict)  # extras
    stdin_payload: str | None = None      # prompt piped on stdin (ARG_MAX safety)
    denied_environment: tuple[str, ...] = ()  # account-only auth stripping


class ExecutionHost(Protocol):
    async def spawn(self, plan: ProcessPlan) -> RunningProcess: ...
```

- **`LocalHost`** — POSIX: `start_new_session=True`; the child env is `os.environ` merged with `plan.environment` minus `plan.denied_environment`; terminates the process group with SIGTERM, then SIGKILL after a grace period. Windows Job Objects come with Windows-native support, later.
- **`DockerExecHost`** — `docker exec -i` against a named, already-running container; `plan.environment` becomes `-e` flags. Container provisioning, teardown, networking, and auth mounts stay owned by the caller (`flywheel-container`); the host only executes plans inside it. It cannot unset image-baked env (`denied_environment` is a no-op there) — in-container auth hygiene stays with the container lifecycle owner's auth guard. Run-level wall-clock ceilings live on `RunRequest.timeout_seconds`, enforced by the runtime.

Commands are always argv arrays; no shell strings unless the plan explicitly opts in.

SDK-based transports (§ 11) bypass hosts — the SDK owns its subprocess — and are therefore local-only. Each transport declares which hosts it supports.

## 8. Normalized event model

The most important stable interface. Every event is a frozen dataclass with a runtime-assigned monotonic `sequence` per run.

```python
class EventType(StrEnum):
    SESSION_STARTED = "session.started"
    SESSION_FINISHED = "session.finished"

    ASSISTANT_MESSAGE = "message.assistant"
    ASSISTANT_DELTA = "message.assistant.delta"
    USER_MESSAGE = "message.user"
    THOUGHT = "thought"

    TOOL_CALL_STARTED = "tool.started"
    TOOL_CALL_UPDATED = "tool.updated"
    TOOL_CALL_FINISHED = "tool.finished"    # payload carries is_error

    FILE_CHANGED = "file.changed"
    COMMAND_STARTED = "command.started"
    COMMAND_FINISHED = "command.finished"

    PLAN_UPDATED = "plan.updated"

    APPROVAL_REQUESTED = "approval.requested"
    APPROVAL_RESOLVED = "approval.resolved"
    PERMISSION_DENIED = "permission.denied"

    CONTEXT_USAGE = "context.usage"
    RATE_LIMITED = "rate.limited"           # payload carries resets_at
    MODE_CHANGED = "mode.changed"

    WARNING = "warning"
    ERROR = "error"
    RAW = "raw"
```

`RATE_LIMITED`, `PERMISSION_DENIED`, and error-carrying `TOOL_CALL_FINISHED` are additions over the original spec: they are the inputs `docs/loop.md`'s detection table requires (rate-limit ETA, blocked_implicit counter, stuck counter).

**Normalization guarantee:** every native event becomes either a known normalized event or an `EventType.RAW` event. Unknown events are never silently discarded.

**`session.finished` payload:** normalized + native stop reason, `pending_tool_use`, usage counters, `total_cost_usd`, `num_turns`, `is_error`, native subtype, `native_session_id`.

**Liveness:** any event — including `THOUGHT` and `RATE_LIMITED` — is a valid hang-watchdog reset, generalizing today's "any SDK message resets the watchdog" rule to every adapter, including CLI-stream runs where each parsed line yields an event. This restores watchdog coverage on the container path, which is inert today.

## 9. Raw events and sinks

```python
@dataclass(frozen=True, slots=True)
class RawAgentEvent:
    sequence: int
    timestamp: datetime
    adapter_id: str
    protocol: str          # "sdk" | "jsonl" | "jsonrpc" | "acp"
    stream: str            # "stdout" | "stderr" | "adapter" | ...
    native_type: str | None
    data: object


class EventSink(Protocol):
    def on_raw(self, event: RawAgentEvent) -> None: ...
    def on_event(self, event: AgentEvent) -> None: ...
```

Raw events are delivered to the sink before normalization; raw and normalized events share one runtime-assigned monotonic sequence. The package ships a null sink and an in-memory sink for tests. **Status:** flywheel's bridge (§ 15.1) forwards *normalized* events into the run telemetry via the harness `on_message` observer; raw envelopes reach telemetry only when a caller wires a dedicated sink — double-recording every CLI line was deliberately skipped in v1.

## 10. Process management

- Full process-tree termination is a correctness requirement: signal the group (SIGTERM, then SIGKILL after grace), await the reader/stdin tasks in `wait()`, fold the exit. No detached tasks survive a run — reader tasks are owned by the process handle and always awaited, rather than by a literal `TaskGroup`.
- WSL2: the runtime runs in the same environment as the agents; cross-boundary execution is a later execution-host variant.
- Executable resolution: `command_override` wins; otherwise the installed executable from `PATH`. A managed (`npx`-pinned) fallback is future work, not implemented.

## 11. Transports

Transports solve protocol mechanics and contain no product behavior. They consume `RunningProcess` streams from a host rather than owning spawns (SDK transports excepted).

- **JSONL (shipped, inline)** — NDJSON line handling lives in the claude-code CLI normalizer (`claude_code/_cli.py`): tolerates interleaved non-JSON lines and malformed/deeply-nested JSON (preserved as RAW, never dropped). It gets extracted into a standalone `JsonLinesTransport` class when a second stream-JSON agent (e.g. Amp) needs it — one consumer does not justify the layer.
- **`JsonRpcTransport` (phase 5, not built)** — request ids, notifications, dispatch, timeouts, cancellation. Codex app server.
- **`AcpTransport` (phase 5, not built)** — Agent Client Protocol client: initialization, session creation, prompts, mode/model selection, permission requests, cancellation, plan and tool events. Gemini CLI, Qwen Code, and future ACP agents share it; per-agent subclasses supply command, flags, config paths, and login probes only.

## 12. Approvals (narrowed)

Normalized `APPROVAL_REQUESTED` events plus one policy callback:

```python
ApprovalResolver = Callable[[ApprovalRequest], Awaitable[ApprovalDecision]]
```

Defaults: timeout, disconnect, and shutdown all resolve to **deny**. flywheel supplies an auto-resolver consistent with its permission policy (under `AUTO`, adapters launch in the agent's native bypass mode, so approval traffic is not expected). No approval queue, UI, or persistence.

**Status:** the event types are reserved in `EventType`; the `ApprovalResolver` callback is not built. It arrives with the first transport that actually surfaces approvals (ACP's `request_permission`, phase 5) — under today's AUTO-only flywheel usage there is nothing to resolve.

## 13. Authentication, environment, and MCP

### 13.1 Probe

```python
@dataclass(frozen=True, slots=True)
class AgentProbeResult:
    installed: bool
    executable_path: Path | None
    version: str | None
    authentication_kind: AuthenticationKind   # account-session | api-key | mixed | installation-only | unauthenticated | unknown
    authentication_assurance: AssuranceLevel  # verified | strong-indication | best-effort | unknown
    authentication_evidence: tuple[str, ...]
    config_paths: tuple[Path, ...]
    warnings: tuple[str, ...]
```

Never claim subscription billing is guaranteed when it cannot be verified. `flywheel init`'s auth report (`_report_agent_auth`) becomes a `probe()` call.

### 13.2 Auth material propagation

Generalizes `flywheel_container.ClaudeAuth` — the taxonomy is already agent-generic, only the names are Claude's:

| Mode | Mechanism | Claude mapping today |
| --- | --- | --- |
| `token-env` | vendor token env var injected | `CLAUDE_CODE_OAUTH_TOKEN` |
| `session-mount` | vendor config/credential dir mounted or inherited | `~/.claude` bind-mount |
| `api-key` | raw API key env var | `ANTHROPIC_API_KEY` |
| `none` | environment pre-authenticated | pre-baked image |

Adapters declare their env-var names and config dirs; the guard against a subscription mode being silently shadowed by a present API key (today `ClaudeAuth.resolve`) becomes generic.

**Status:** shipped for claude-code specifically — `ACCOUNT_ONLY` strips `ANTHROPIC_API_KEY` on the CLI transport (`denied_environment`) and raises `AuthenticationPolicyError` on the SDK transport (whose subprocess inherits the parent env); the probe reports the mixed-credentials strand warning. The *generalized* per-adapter declaration and the `ClaudeAuth` replacement in flywheel-container are future work (§ 16 Later).

### 13.3 Environment policy

Target: child environments constructed, not inherited — base safe set (`HOME`, `PATH`, `SHELL`, `TMPDIR`, locale, XDG, `SSH_AUTH_SOCK`, `GIT_SSH_COMMAND`) + adapter-declared credential vars + request vars − denied vars.

**Status: not built; deliberate.** v1 matches current flywheel behavior for parity: `LocalHost` inherits `os.environ` merged with request extras minus `denied_environment` (exactly how the agent SDK behaves today), and `DockerExecHost` leaves the base env to the image. The constructed-env hardening is a standalone future change because it alters what every existing run inherits.

### 13.4 MCP

One canonical `McpServerConfig` (name, command, args, environment, url, transport) that adapters translate into their native launch configuration (`mcpServers`, `mcp_servers`, ...). Config-**file** writing is out of scope for v1; flywheel's committed repo `.mcp.json` remains the operative mechanism.

**Status: not built** (arrives with the second agent, where translation becomes real). Today `mcp_servers`/`mcp_strict` pass through claude-code `adapter_options` onto the SDK transport verbatim.

## 14. Errors, registry, versioning

- Error hierarchy: `AgentNotInstalledError`, `AgentAuthenticationError`, `AuthenticationPolicyError`, `UnsupportedCapabilityError`, `AgentStartupError`, `AgentProtocolError`, `AgentProcessExitedError`, `ApprovalTimeoutError` — each with `code` and `retryable`. Never include secrets or environment dumps in error payloads.
- Registry discovers third-party adapters via the `flywheel_agents.adapters` entry-point group; a failing plugin is reported (`load-failed` + error) and never prevents the runtime from loading.
- Each adapter exposes `AdapterCompatibility` (minimum / maximum-tested / pinned versions). The startup warning outside the tested range is not wired yet — the claude adapter declares no bounds today (§ 16 Later).

## 15. Flywheel integration

The retrofit surface. The seam already exists: `InvokeFunc = Callable[[InvocationRequest], Awaitable[IterationResult]]` (`flywheel_core.harness`), with two production implementations today — the SDK closure (`workflow._make_claude_code_invoke`) and the container stream path (`flywheel_container`). Both are replaced by one bridge.

### 15.1 The bridge

A `flywheel_core` module (the successor to `_sdk.py` as the optional-dependency boundary) that converts `InvocationRequest -> RunRequest` and `CompletedRun -> IterationResult`. Envelope parsing (`parse_envelope`) is unchanged — the LOOP_STATUS contract is already agent-agnostic.

| `InvocationSignals` field | Source |
| --- | --- |
| `stop_reason` / `result_subtype` | `StopInfo` (normalized + native) |
| `num_turns`, `total_cost_usd` | `CompletedRun` |
| `result_is_error`, `api_error_status` | `session.finished` payload / `RunFailure` |
| `session_id` | `native_session_id` |
| `tool_interactions` | `TOOL_CALL_*` events |
| `permission_denials` | `PERMISSION_DENIED` events |
| `rate_limit_events` | `RATE_LIMITED` events |
| `pending_tool_use_at_stop` | `StopInfo.pending_tool_use` |
| `usage` (message-less path today) | `CompletedRun.usage` |
| `on_message` watchdog resets | any normalized event |
| `context_observer` | `CONTEXT_USAGE` events |
| `recovery_interrupt_event`, `set_model`, `say` | mid-turn capabilities (gated) |

**v1 deviations from this table** (each documented on the bridge itself): `rate_limit_events` is left empty (the signals field is typed with SDK event objects; session-limit refusals still surface via `CompletedRun.fault` and the transcript regexes in `flywheel_core.faults`); `context_observer` is not forwarded, so mid-turn occupancy tracking degrades exactly as it does on the container path today; the mid-turn control rows are inert because the shipped claude adapter declares those capabilities `False`; `transcript_graders` are not enforced mid-run (grade-time enforcement unchanged).

### 15.2 Invocation sites routed through the runtime

All seven independent `ClaudeAgentOptions` construction sites converge — the worker invoker (`workflow.py`), the rubric judge (`grader_rubric.py`), the recovery summarizer (`recovery_summarizer.py`), the merge-conflict recovery agent (`worker.py`), and autopilot's invokers (`_autopilot.py`). **Status: shipped, opt-in.** When `[agent] id` is set, every site routes through the runtime (`make_agents_invoke`, `make_agents_judge_invoke`, `make_agents_summarizer_invoke`, the worker's agents-backed `ConflictResolver`, autopilot's runtime-backed invokers); unset keeps each site's legacy SDK default byte-identical. Autopilot's subagent-bearing session has no cross-agent equivalent and routes through the runtime (SDK transport, `agents` adapter option) only for `agent_id = "claude-code"` — any other agent id keeps that one stage on the legacy Claude path, by design.

### 15.3 Config surface

- `[agent] id = "claude-code"` (new, default preserves behavior); `model` stays an opaque per-agent string. Shipped, plus `[agent] transport`.
- `permission_mode` maps to `PermissionPolicy` at the workflow boundary (never silently to bypass); the `[sandbox]` key itself keeps Claude vocabulary for back-compat. Shipped.
- `[sandbox.capabilities]` rides claude-code `adapter_options` (it is SDK vocabulary); the SDK transport consumes it, the CLI transport has no flag surface for it — the same host-SDK-only scoping `docs/sandbox.md` documents. Shipped.
- `[sandbox.container] auth` / `auth_env` generalizing to the per-agent auth modes of § 13.2: future (`ClaudeAuth` unchanged today).

### 15.4 Container backend rebase

`flywheel-container` keeps sandbox provisioning, teardown, networking, UID preflight, and auth mounts. Its invocation path — `ClaudeCliAgent.build_command` and `_stream.py` — is deleted in favor of the claude-code adapter's CLI-stream transport under `DockerExecHost`. This closes documented v1 limitations for free: the hang watchdog and per-message telemetry go live in containers because every parsed stream line becomes a normalized event.

### 15.5 Other retrofits

- `faults.py` session-limit regexes are ported into the claude-code adapter's `classify_fault` (shipped). They are duplicated, not moved: `flywheel_core.faults` still owns the legacy path and the clock-time reset derivation; deleting them there comes with the default flip.
- `CompletedRun.native_session_id` populating the dead `lifecycles.session_id` column: **not done** — the harness never persists it on any path; needs a harness-side change (§ 16 Later).
- `docs/container-backend.md` limitations updated (shipped). Re-expressing `docs/loop.md`'s detection table in normalized events with per-capability degradation: **not done**, deferred to the default flip (§ 16 Later).

## 16. Delivery phases

Old paths stay default until in-loop parity is proven; the in-loop verification gate (`docs/loop.md`) applies to every phase that touches the loop.

1. **Foundation — SHIPPED.** Models, events, fold, adapter contract, hosts, fake-agent executable, contract test suite. No real-agent dependence in CI.
2. **claude-code adapter — SHIPPED.** Both transports (SDK, CLI stream-json); cross-transport fold parity proven by test (the same logical exchange normalized via both transports folds to agreeing `CompletedRun`s).
3. **Core bridge — SHIPPED (opt-in).** `[agent] id` / `[agent] transport` route the worker invoke through the runtime; the legacy SDK closure remains the default. All seven invocation sites now follow the same opt-in (§ 15.2).
4. **Container rebase — SHIPPED.** `DockerExecHost` + the claude-code CLI transport replaced `ClaudeCliAgent`/`_stream.py`; the hang watchdog and per-event telemetry are live in containers; exec timeout and nonzero exits fold to structured failures.
5. **Second agent — SHIPPED (codex, headless JSONL).** The codex adapter drives `codex exec --json` under any execution host: thread/turn/item normalization (agent messages, reasoning, command executions with exit codes, file changes, MCP calls, web search, todo plans), sandbox-mode permission mapping (including a real `read-only`), `OPENAI_API_KEY` account-only stripping, and probe. The § 18 agent-swap acceptance test passes: one branch-free orchestration helper runs both adapters. **Caveats:** built against the documented stream schema with the RAW-fallback guarantee — the codex CLI is not installed on the dev machine, so validation against a live binary is still owed; the JSON-RPC app-server transport (approvals, mid-turn control) remains future work; `classify_fault` deliberately recognizes nothing yet.

Later (consolidated backlog, each item cross-referenced where it is specified): live-binary validation of the codex adapter and its JSON-RPC app-server transport (§ 16 phase 5 caveats); the default flip away from the legacy SDK closure after in-loop parity, with the `faults.py` regex removal and the `loop.md` detection-table rewrite (§ 15.5); persisting `native_session_id` onto `lifecycles.session_id` (§ 15.5); generalized auth propagation replacing `ClaudeAuth` and auth-policy wiring into `init` (§ 13.2); constructed-environment hardening (§ 13.3); the canonical MCP schema (§ 13.4); the `ApprovalResolver` callback (§ 12); standalone JSON-RPC/ACP transport classes (§ 11); managed-executable (`npx`) fallback and compatibility-range warnings (§ 10, § 14); rate-limit and context-usage forwarding through the bridge (§ 15.1); option discovery surfaces; additional agents (Gemini/Qwen via ACP); a service layer via the interactivity program.

## 17. Testing strategy

- **Contract tests** every adapter must pass: probe does not crash; start emits `session.started`; a prompt produces events; completion emits `session.finished`; cancel kills the tree; unknown native events surface as RAW; stderr is captured; cwd and environment policy are respected; unsupported capabilities fail explicitly.
- **Fake agent** — a deterministic test executable emulating JSONL output, delays, malformed JSON, partial messages, native session ids, crashes, hangs, child spawning, and cancellation refusal. CI never depends on real vendor CLIs.
- **Fixtures** — normalizers and the fold are testable entirely offline. Shipped as authored stream-json/SDK-object fixtures plus the cross-transport parity test; recording sanitized transcripts from real runs is the stronger future form and lands with the default flip's parity evidence.
- **Real integration tests** — conditional on installed infrastructure; never mandatory in CI. Shipped for docker (the container package's live tests); a conditional real-`claude` run is future work.

## 18. Acceptance criteria

An application (flywheel's loop) changes `agent_id` from `"claude-code"` to `"codex"` or `"gemini-cli"` without changing orchestration logic. Differences appear only through capabilities, option discovery, event availability, and authentication assurance. (Evaluable once a second adapter exists — phase 5.)

Flywheel-specific (all four hold as of phases 1-4):

1. `import flywheel_core` works with no agent extras installed (`agents_invoke` imports `flywheel_agents` lazily, mirroring `_sdk`).
2. The container backend runs through the same claude-code adapter as the worktree backend, with a live hang watchdog; `flywheel_container._stream` and `ClaudeCliAgent` are deleted.
3. Loop-guard behavior on the default SDK path is unchanged — that path is byte-identical (untouched); the agents path is opt-in.
4. The LOOP_STATUS envelope contract and its parser are untouched.

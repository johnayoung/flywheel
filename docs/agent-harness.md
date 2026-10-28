# flywheel-agents — the multi-agent execution layer

**Status: shipped; the only agent path.** Architecture and status record for the `flywheel-agents` crate (`crates/flywheel-agents`). Every agent invocation in flywheel runs through this runtime; `[agent] id` selects the adapter (default `claude-code`). There is no in-process vendor SDK: agents run as their own CLIs. Sections carry explicit **Status** markers wherever the built code deviates from or defers the text; where an unmarked claim and shipped code disagree, the code's own spec set (`docs/README.md`) wins.

## 1. Objective

One consistent way to run coding-agent harnesses — Claude Code, OpenAI Codex, Gemini CLI, Qwen Code, OpenCode, Amp, and others — as the brain inside flywheel's loop. The crate invokes the **real agent harnesses**, preserving their native authentication, subscription plans, tools, configuration, MCP servers, model selection, and permission systems.

It is **not**: an LLM router, a model API abstraction, an OpenAI-compatible proxy, a common re-implementation of coding-agent behavior, or a TUI screen-scraper.

```text
flywheel loop (InvokeFn seam)
    │
    ▼
flywheel-agents runtime  ── normalized events + CompletedRun fold
    │
    ├── claude-code adapter ── CLI stream-json, bidirectional control protocol (any host)
    ├── codex adapter ──────── headless exec JSONL (app-server JSON-RPC later)
    ├── gemini adapter ─────── ACP (backlog)
    ├── qwen adapter ────────── ACP (backlog)
    └── other adapters ─────── best available structured interface
```

The crate standardizes the **control plane**, not the agent implementation.

### Position in the workspace

Bottom-of-stack crate: `crates/flywheel-agents`. It depends on **no** flywheel crate (runtime deps: `serde`, `serde_json`, `thiserror`, `chrono`, `tokio`, `regex`, `uuid`, `libc`). `flywheel-core` depends on it directly; the single agent-driving boundary in core is `crates/flywheel-core/src/agents_invoke.rs` (§ 15).

Named `flywheel-agents`, not `flywheel-harness`: "harness" is already the loop harness (`crates/flywheel-core/src/harness/`); this doc uses "agent harness" only when referring to the vendor CLIs themselves.

## 2. Requirements

### 2.1 v1 core requirements

1. Start a run and stream normalized events.
2. Run to completion and return a structured fold (`CompletedRun`) — flywheel's unit of work is one loop iteration, drained to completion.
3. Cancel a running agent; reliably terminate the full process tree.
4. Preserve every raw native event before normalization.
5. Report per-adapter capabilities honestly.
6. Probe installation and authentication status.
7. Select working directory; shape the child environment per policy (extras + denials; constructed-env hardening later, § 13.3).
8. Drive the same adapter under a local subprocess **or** `docker exec` (execution-host seam, § 7).
9. Run sessions concurrently; Linux, macOS, WSL2 (Windows-native later).
10. Prefer the agent's account-plan authentication wherever supported.

### 2.2 Explicit v1 non-requirements

- Feature parity across agents; prompt translation; replicating native tools; managing subscriptions; parsing interactive TUIs.
- **An HTTP/WebSocket service.** Library only. A service surface belongs to the interactivity program and wraps this runtime later.
- **Crate-owned persistence.** No SQLite, no `~/.agent-harness`. The crate emits through a sink trait (§ 9); flywheel's telemetry sink and relational store are the system of record.
- **Interactive approvals.** flywheel runs unattended. Normalized approval events plus an auto-resolving policy callback (§ 12); no approval UI or queue subsystem.
- **Profiles.** `flywheel.toml` is the profile layer.
- **MCP config-file writing.** Canonical read/translate only (§ 13).
- **Session resume/fork/replay as a product feature.** Fresh-session-per-iteration is a deliberate flywheel design choice; `native_session_id` is captured and reported. The claude-code adapter accepts `resume`/`session_id`/`fork_session` adapter options, used only for envelope salvage (§ 15.1).

## 3. Design principles

1. **Native harness first.** Use the deepest structured interface available: official structured protocol > ACP > JSON-RPC > NDJSON/stream JSON > structured one-shot output > plain text. TUI automation is not a supported integration.
2. **Preserve native behavior.** Adapters never reimplement file editing, shell execution, repo indexing, model calls, compaction, or native subagents.
3. **Capability honesty.** Expose what each adapter actually implements; never imply parity. flywheel's loop guards degrade explicitly per capability, not silently.
4. **Raw events are immutable.** Every native event is retained before normalization — debugging, replay, normalizer migration, audit.
5. **Adapters translate; the runtime orchestrates.** Agent-specific code lives in adapters. Process lifecycle, event fan-out, cancellation, environment and auth policy live in the runtime.
6. **Storage-free.** Emit, don't persist. All durable state belongs to the caller.
7. **argv construction and process spawning are separate axes.** Adapters produce a `ProcessPlan`; execution hosts spawn it. This is what lets one adapter serve both the worktree and container backends.
8. **Run-to-completion is first-class.** The streaming API (`AgentRuntime::start`) exists, but the supported product contract is the `CompletedRun` fold (§ 6.2).

## 4. Crate structure

```text
crates/flywheel-agents/
├── Cargo.toml
├── src/
│   ├── lib.rs                 # the public surface (re-exports), BoxFuture
│   ├── adapter.rs             # AgentAdapter / RunningAgent / AgentControl / AdapterServices / approvals
│   ├── capabilities.rs
│   ├── config.rs              # AgentConfiguration / PermissionPolicy / AuthenticationPolicy
│   ├── errors.rs              # AgentError
│   ├── events.rs              # EventType / AgentEvent / RawAgentEvent / sinks / RunRecorder
│   ├── fold.rs                # event stream -> CompletedRun
│   ├── hosts.rs               # ProcessPlan / LocalHost / DockerExecHost
│   ├── jsonl.rs               # shared JSON-lines agent + control channel
│   ├── models.rs
│   ├── probe.rs
│   ├── registry.rs            # AdapterRegistry, default_registry()
│   ├── runtime.rs             # AgentRuntime::run / ::start
│   ├── claude_code/
│   │   ├── mod.rs             # adapter + transport routing (cli only)
│   │   ├── cli.rs             # stream-json plan + normalizer (any host)
│   │   ├── control.rs         # mid-turn control_request frames
│   │   ├── common.rs          # shared usage/stop normalization
│   │   ├── probe.rs
│   │   └── faults.rs          # session-limit fault classification
│   └── codex/
│       ├── mod.rs             # cli transport only (JSON-RPC later)
│       ├── cli.rs             # codex exec --json plan + normalizer
│       └── probe.rs
└── tests/                     # contract suites, normalizer/plan/host/probe tests, scripted fake agents (tests/common)
```

ACP / JSON-RPC transport modules, the auth-policy subsystem, and the MCP canonical schema arrive with their backlog items (§ 16), as do additional adapters (`gemini/`, `qwen/`, ...).

## 5. Domain model

Plain Rust structs and enums with `serde` (de)serialization; enums serialize kebab-case. Async trait methods return `BoxFuture` (a boxed `Send` future) so the adapter and host traits stay object-safe. Full definitions: `src/models.rs`, `src/capabilities.rs`, `src/config.rs`.

### 5.1 Agent identity

```rust
pub struct AgentDescriptor {
    pub id: String,                  // "claude-code", "codex", ...
    pub display_name: String,
    pub adapter_version: String,
    pub vendor: Option<String>,
    pub executable_names: Vec<String>,
    pub capabilities: AgentCapabilities,
    pub compatibility: AdapterCompatibility,
}
```

Agent ids are open strings, never a closed enum — additional adapters register into an `AdapterRegistry` at compile time (§ 14).

### 5.2 Capabilities

Grouped by what consumes them. The **signal fidelity** group is the loop-guard input map: each flag corresponds to a detection row in `docs/loop.md`. Every flag defaults to `false` except `cancellation`.

```rust
pub struct AgentCapabilities {
    // session
    pub native_resume: bool,
    pub native_fork: bool,
    // control
    pub cancellation: bool,                 // default true
    pub mid_turn_interrupt: bool,
    pub mid_turn_model_change: bool,        // control-command set_model
    pub mid_turn_prompt_injection: bool,    // control-command say
    pub approvals: bool,
    // selection / discovery
    pub model_selection: bool,
    pub model_discovery: bool,
    pub mode_selection: bool,
    pub reasoning_selection: bool,
    // signal fidelity (loop-guard inputs)
    pub structured_tool_calls: bool,        // stuck/thrash counters
    pub tool_result_errors: bool,           // blocked_implicit counter
    pub thought_events: bool,               // hang-watchdog liveness
    pub context_usage: bool,                // context_exhausted detection
    pub rate_limit_events: bool,            // rate_limited flag + ETA
    pub hook_events: bool,
    // ecosystem
    pub subagents: bool,
    pub mcp_configuration: bool,
    pub slash_commands: bool,
    // auth
    pub account_login_detection: bool,
    pub api_key_detection: bool,
}
```

Capabilities describe the **adapter implementation**, not theoretical vendor functionality. Shipped declarations: claude-code sets everything except `model_discovery`, `mode_selection`, `hook_events` (hook frames are forwarded RAW), and `slash_commands`; codex sets `cancellation`, `model_selection`, `structured_tool_calls`, `tool_result_errors`, `thought_events`, and both auth-detection flags.

### 5.3 Configuration

```rust
pub enum PermissionPolicy { Auto, Supervised /* default */, Plan, ReadOnly }
// Auto = flywheel's mode: translated to the agent's native bypass

pub enum AuthenticationPolicy { AccountPreferred /* default */, AccountOnly, ApiAllowed }

pub struct AgentConfiguration {
    pub agent_id: String,
    pub model_id: Option<String>,
    pub mode_id: Option<String>,
    pub reasoning_id: Option<String>,
    pub permission_policy: PermissionPolicy,
    pub authentication_policy: AuthenticationPolicy,
    pub max_turns: Option<u32>,
    pub command_override: Option<Vec<String>>,
    pub environment: BTreeMap<String, String>,
    pub adapter_options: BTreeMap<String, serde_json::Value>,
}
```

`adapter_options` carries what the generic fields cannot honestly represent. For claude-code this is where the `[sandbox.capabilities]` vocabulary lands — `skills`, `allowed_tools`, `disallowed_tools`, `setting_sources`, `mcp_servers`, `mcp_strict`, `sandbox_exec` — plus `transport`, `input_mode`, `agents`, `settings`, `hooks`, session options, and the co-author suppression toggle (full list in the `claude_code/cli.rs` module docs). Adapters reject a permission policy they cannot honor (claude-code has no `ReadOnly`) rather than approximating.

## 6. Adapter contract

```rust
pub trait AgentAdapter: Send + Sync {
    fn descriptor(&self) -> &AgentDescriptor;
    fn probe(&self) -> BoxFuture<'_, AgentProbeResult>;
    fn discover_options(&self, context: DiscoveryContext) -> BoxFuture<'_, AgentOptions>;
    fn start(&self, request: RunRequest, services: AdapterServices)
        -> BoxFuture<'_, Result<Box<dyn RunningAgent>, AgentError>>;
    fn classify_fault(&self, _evidence: &FaultEvidence) -> Option<AgentFault> { None }
}
```

`classify_fault` lets adapters own recognition of vendor-specific fault strings (e.g. `"Claude AI usage limit reached|<epoch>"`) and map them to a normalized taxonomy (`session_limit` with `resets_at_epoch`, `auth`, `quota`, `infra`).

### 6.1 Running agent

```rust
pub trait RunningAgent: Send {
    fn native_session_id(&self) -> Option<String>;
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<AgentEvent>, AgentError>>;
    fn control(&self) -> Option<Arc<dyn AgentControl>>;
    fn cancel(&mut self) -> BoxFuture<'_, ()>;
    fn wait(&mut self) -> BoxFuture<'_, AgentExit>;
}

pub trait AgentControl: Send + Sync {
    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>>;
    fn set_model(&self, model: Option<String>) -> BoxFuture<'_, Result<(), AgentError>>;
    fn set_permission_policy(&self, policy: PermissionPolicy) -> BoxFuture<'_, Result<(), AgentError>>;
    fn inject_message(&self, text: String) -> BoxFuture<'_, Result<(), AgentError>>;
    fn context_usage(&self) -> BoxFuture<'_, Result<Value, AgentError>>;
}
```

Mid-turn control is capability-gated: a transport that cannot steer returns `None` from `control()`. **Status: shipped for claude-code** — the default `input_mode = "stream-json"` keeps stdin open and drives the CLI's `control_request` protocol (`interrupt`, `set_model`, `set_permission_mode`, `get_context_usage`, user-message injection; `claude_code/control.rs`). `input_mode = "text"` (prompt piped as plain text, stdin closed) and codex (one-way headless exec) expose no control.

### 6.2 The fold: `CompletedRun`

The supported contract for flywheel. The runtime drains the event stream and folds it (`AgentRuntime::run`, `src/runtime.rs`; `EventFolder`, `src/fold.rs`):

```rust
pub struct CompletedRun {
    pub final_text: String,                       // concatenated assistant text
    pub stop: StopInfo,                           // normalized reason + native string + subtype + pending_tool_use + finished
    pub usage: Option<BTreeMap<String, i64>>,     // token counters
    pub total_cost_usd: Option<f64>,
    pub num_turns: Option<i64>,
    pub native_session_id: Option<String>,
    pub tool_interactions: Vec<ToolInteraction>,
    pub permission_denials: Vec<PermissionDenial>,
    pub rate_limit_events: Vec<RateLimitInfo>,
    pub fault: Option<AgentFault>,
    pub failure: Option<RunFailure>,              // transport/process failure, not agent output
    pub exit: AgentExit,
    pub event_count: u64,
}

pub enum StopReason { Completed, MaxTokens, MaxTurns, Cancelled, Error, Unknown }
```

Native stop strings are preserved alongside the normalized enum. `StopInfo.finished` is true only when the agent emitted its terminal envelope (a stream-json `result`, a codex `turn.completed`).

## 7. Execution hosts

Adapters build a `ProcessPlan`; hosts spawn it (`src/hosts.rs`).

```rust
pub struct ProcessPlan {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,   // extras
    pub stdin: StdinPlan,                        // Closed | Payload(prompt) | Open (control protocol)
    pub denied_environment: Vec<String>,         // account-only auth stripping
}

pub trait ExecutionHost: Send + Sync {
    fn spawn(&self, plan: ProcessPlan) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, AgentError>>;
}
```

- **`LocalHost`** — POSIX: the child is spawned as a process-group leader; its env is the parent environment merged with `plan.environment` (extras win) minus `plan.denied_environment`; `kill` sends SIGTERM to the group, then SIGKILL after a grace period (`DEFAULT_KILL_GRACE`, 5 s). Dropping an unreaped handle SIGKILLs the group, and every live group is registered in `process_groups` so worker/autopilot/triage shutdown sweeps them. Windows Job Objects come with Windows-native support, later.
- **`DockerExecHost`** — `docker exec -i` against a named, already-running container; `plan.environment` becomes `-e` flags. Container provisioning, teardown, networking, and auth mounts stay owned by the caller (`flywheel-container`); the host only executes plans inside it. The agent runs under a `/bin/sh` wrapper that records its pgid in a per-run `/tmp` file; kill signals that group via a second `docker exec` (killing the local client alone would leave the agent running in the container). It cannot unset image-baked env (`denied_environment` is a no-op there) — in-container auth hygiene stays with the container lifecycle owner's auth guard. Run-level wall-clock ceilings live on `RunRequest.timeout_seconds`, enforced by the runtime.

Commands are always argv arrays, never shell strings. Stdout is line-framed with a 64 MiB per-line ceiling (`LINE_LIMIT_BYTES`; overrun is an `AgentProtocol` error); stderr keeps a bounded 64 KiB tail.

## 8. Normalized event model

The most important stable interface. Every event carries a runtime-assigned monotonic `sequence` per run (`src/events.rs`). Wire names:

```text
session.started   session.finished
message.assistant message.assistant.delta  message.user  thought
tool.started      tool.updated   tool.finished      # tool.finished payload carries is_error
file.changed      command.started  command.finished
plan.updated
approval.requested  approval.resolved  permission.denied
context.usage     rate.limited  (payload carries resets_at)   mode.changed
warning  error  raw
```

`rate.limited`, `permission.denied`, and error-carrying `tool.finished` are the inputs `docs/loop.md`'s detection table requires (rate-limit ETA, blocked_implicit counter, stuck counter).

**Normalization guarantee:** every native event becomes either a known normalized event or a `raw` event. Unknown events are never silently discarded.

**`session.finished` payload:** normalized + native stop reason, `pending_tool_use`, usage counters, `total_cost_usd`, `num_turns`, `is_error`, native subtype, `native_session_id`.

**Liveness:** any event — including `thought` and `rate.limited` — is a valid hang-watchdog reset, on every adapter and host, because each parsed stream line yields an event. The watchdog itself lives in the core harness, not this crate.

## 9. Raw events and sinks

```rust
pub struct RawAgentEvent {
    pub adapter_id: String,
    pub protocol: String,          // "jsonl" for every shipped transport
    pub stream: String,            // "stdout" | "stderr" | "stdin" | "adapter"
    pub data: Value,
    pub native_type: Option<String>,
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
}

pub trait EventSink: Send + Sync {
    fn on_raw(&self, event: &RawAgentEvent);
    fn on_event(&self, event: &AgentEvent);
}
```

Raw events are delivered to the sink before normalization; raw and normalized events share one monotonic sequence (`RunRecorder`). The crate ships `NullSink` and an in-memory `MemorySink` for tests. **Status:** flywheel's bridge (§ 15.1) forwards *normalized* events into the run telemetry via the harness `on_message` observer; raw envelopes reach telemetry only when a caller wires a dedicated sink (`AgentsInvoker::with_sink`).

## 10. Process management

- Full process-tree termination is a correctness requirement: signal the group (SIGTERM, then SIGKILL after grace), join the reader/writer tasks in `wait()`, fold the exit. Reader tasks are owned by the process handle and always joined; dropping an `ActiveRun` cancels the run.
- WSL2: the runtime runs in the same environment as the agents; cross-boundary execution is a later execution-host variant.
- Executable resolution: `command_override` wins; otherwise the installed executable from `PATH`. A managed (`npx`-pinned) fallback is future work, not implemented.

## 11. Transports

Transports solve protocol mechanics and contain no product behavior. They consume `RunningProcess` streams from a host rather than owning spawns.

- **JSON lines (shipped)** — `src/jsonl.rs`: the shared `JsonLinesAgent` plus `ControlChannel` (request ids, responses, timeouts for the claude-code `control_request` protocol, inbound `can_use_tool` approval requests). Tolerates interleaved non-JSON lines and malformed JSON (preserved as `raw`, never dropped). Both shipped adapters ride it with their own `LineNormalizer`.
- **`[agent] transport`** — `cli` is the only transport for both adapters. `transport = "sdk"` (the former in-process Python SDK path) is rejected: the claude-code adapter fails the run with `UnsupportedCapability`, and the policy loader rejects it at config load. The CLI transport speaks the same stream-json control protocol the vendor SDKs wrap, so nothing is lost.
- **JSON-RPC (backlog, not built)** — request ids, notifications, dispatch, timeouts, cancellation. The codex app server (the shipped codex adapter uses headless exec JSONL instead).
- **ACP (backlog, not built)** — Agent Client Protocol client: initialization, session creation, prompts, mode/model selection, permission requests, cancellation, plan and tool events. Gemini CLI, Qwen Code, and future ACP agents share it.

## 12. Approvals (narrowed)

Normalized `approval.requested` / `approval.resolved` events plus one policy callback (`src/adapter.rs`):

```rust
pub type ApprovalResolver =
    Arc<dyn Fn(ApprovalRequest) -> BoxFuture<'static, ApprovalDecision> + Send + Sync>;
// ApprovalDecision::Allow { updated_input } | ApprovalDecision::Deny { message, interrupt }
```

The default (`deny_all_resolver`) denies every request; an approval the resolver does not answer within `approval_timeout_seconds` (default 60 s) resolves to deny. Under `Auto`, adapters launch in the agent's native bypass mode, so approval traffic is not expected. No approval queue, UI, or persistence.

**Status:** shipped for claude-code's stream-json input mode (inbound `can_use_tool` requests are routed to the resolver and answered on stdin). Every flywheel call site uses the default deny resolver.

## 13. Authentication, environment, and MCP

### 13.1 Probe

```rust
pub struct AgentProbeResult {
    pub installed: bool,
    pub executable_path: Option<PathBuf>,
    pub version: Option<String>,
    pub authentication_kind: AuthenticationKind,     // account-session | api-key | mixed | installation-only | unauthenticated | unknown
    pub authentication_assurance: AssuranceLevel,    // verified | strong-indication | best-effort | unknown
    pub authentication_evidence: Vec<String>,        // names and paths only, never secret values
    pub config_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
}
```

Never claim subscription billing is guaranteed when it cannot be verified. Shipped probes: claude-code and codex. `flywheel init`'s auth report (`report_agent_auth`, `crates/flywheel-orchestrator/src/workflow/init.rs`) is separate; routing it through `probe()` is future work (§ 16 Later).

### 13.2 Auth material propagation

`ClaudeAuth` (`crates/flywheel-container/src/auth.rs`) carries an agent-generic taxonomy under Claude names:

| Mode | Mechanism | Claude mapping today |
| --- | --- | --- |
| `token-env` | vendor token env var injected | `CLAUDE_CODE_OAUTH_TOKEN` |
| `session-mount` | vendor config/credential dir mounted or inherited | `~/.claude` bind-mount |
| `api-key` | raw API key env var | `ANTHROPIC_API_KEY` |
| `none` | environment pre-authenticated | pre-baked image |

Adapters declare their env-var names and config dirs; the guard against a subscription mode being silently shadowed by a present API key (`ClaudeAuth::resolve`) becomes generic.

**Status:** `AccountOnly` strips the vendor API key through `denied_environment` (`ANTHROPIC_API_KEY` for claude-code, `OPENAI_API_KEY` for codex); the claude probe reports the mixed-credentials warning. The *generalized* per-adapter declaration and the `ClaudeAuth` replacement in flywheel-container are future work (§ 16 Later).

### 13.3 Environment policy

Target: child environments constructed, not inherited — base safe set (`HOME`, `PATH`, `SHELL`, `TMPDIR`, locale, XDG, `SSH_AUTH_SOCK`, `GIT_SSH_COMMAND`) + adapter-declared credential vars + request vars − denied vars.

**Status: not built; deliberate.** `LocalHost` inherits the parent environment merged with request extras minus `denied_environment`, and `DockerExecHost` leaves the base env to the image. The constructed-env hardening is a standalone future change because it alters what every existing run inherits.

### 13.4 MCP

One canonical `McpServerConfig` (name, command, args, environment, url, transport) that adapters translate into their native launch configuration (`mcpServers`, `mcp_servers`, ...). Config-**file** writing is out of scope for v1; flywheel's committed repo `.mcp.json` remains the operative mechanism.

**Status: not built** (backlog — codex MCP config stays in `~/.codex/config.toml`, untouched by the adapter). Today the claude-code adapter passes an `mcp_servers` object (or config path string) to `--mcp-config` and `mcp_strict` to `--strict-mcp-config`.

## 14. Errors, registry, versioning

- One error enum, `AgentError` (`src/errors.rs`): `UnknownAgent`, `AdapterAlreadyRegistered`, `AgentNotInstalled`, `AgentAuthentication`, `AuthenticationPolicy`, `UnsupportedCapability`, `AgentStartup`, `AgentProtocol`, `AgentProcessExited`, `ApprovalTimeout`, `ControlRequest` — each with a stable `code()` and `retryable()`. Never include secrets or environment dumps in error messages.
- Registry: `default_registry()` registers the first-party adapters (claude-code, codex) at compile time (`src/registry.rs`); there is no runtime plugin discovery. An embedder adds adapters with `AdapterRegistry::register` or `register_plugin` (a fallible factory: an error, duplicate id, or panic is recorded on `load_failures` and never prevents the registry from loading).
- Each adapter exposes `AdapterCompatibility` (minimum / maximum-tested / pinned versions). The startup warning outside the tested range is not wired yet — neither shipped adapter declares bounds (§ 16 Later).

## 15. Flywheel integration

The seam is the harness `InvokeFn` (one `InvocationRequest` in, one `IterationResult` out). Its single production implementation is `AgentsInvoker` (`crates/flywheel-core/src/agents_invoke.rs`), driving the runtime under `LocalHost` (worktree backend) or `DockerExecHost` (container backend).

### 15.1 The bridge

`agents_invoke.rs` converts `InvocationRequest -> RunRequest` and `CompletedRun -> IterationResult` (`completed_run_to_iteration_result`). Envelope parsing (`parse_envelope`) is unchanged — the LOOP_STATUS contract is agent-agnostic.

| `InvocationSignals` field | Source |
| --- | --- |
| `stop_reason` / `result_subtype` | `StopInfo` (native + native subtype) |
| `num_turns`, `total_cost_usd` | `CompletedRun` |
| `result_is_error` | `StopInfo.is_error` (only when `finished`) |
| `api_error_status` | recorded `session.finished` message |
| `session_id` | `native_session_id` |
| `tool_interactions` / `tool_result_blocks` | `tool.*` events |
| `permission_denials` | `permission.denied` events (as JSON) |
| `rate_limit_events` | `rate.limited` events (as JSON) |
| `pending_tool_use_at_stop` | `StopInfo.pending_tool_use` |
| `usage` | `CompletedRun.usage` |
| `on_message` watchdog resets | any normalized event |
| `context_observer` | `AgentControl::context_usage`, polled by the in-run control watcher |
| operator `interrupt` / `say` / `set_model`, recovery request, checkpoint nudge | `AgentControl`, driven by the in-run control watcher |

The bridge also enforces transcript-grader hard limits mid-run (`enforce_transcript_limits`, cancelling the run on the first breach) and salvages a lost closing envelope on a cleanly finished iteration by resuming the same native session with `ENVELOPE_SALVAGE_PROMPT`; only a valid re-emitted envelope is adopted. `hook_events` is always empty (no CLI equivalent of SDK hook callbacks).

### 15.2 Invocation sites routed through the runtime

Every agent invocation goes through the runtime: the worker invoker (`AgentsInvoker`), the rubric judge (`AgentsJudgeInvoke`, `crates/flywheel-core/src/grader_rubric.rs`) and recovery summarizer (`AgentsSummarizerInvoke`, `recovery_summarizer.rs`) via the shared `ColdSession`, the merge-conflict recovery agent (`make_agents_resolve_conflict`, `crates/flywheel-worktree/src/daemon.rs`), autopilot's invokers including the single subagent-bearing session (`AgentsSingleSessionRunner`, `crates/flywheel-orchestrator/src/autopilot.rs`, which passes the 11 tier subagents through the `agents` adapter option to `--agents`), and the container backend (§ 15.4). The subagent session is claude-code only; it has no cross-agent equivalent.

### 15.3 Config surface

- `[agent] id` selects the adapter (default `claude-code`); `model` stays an opaque per-agent string. `[agent] transport` requires `[agent] id`; `cli` is the only accepted value (§ 11).
- `permission_mode` maps to `PermissionPolicy` at the workflow boundary (`permission_policy_for`, `crates/flywheel-core/src/workflow.rs`: `bypassPermissions` -> `Auto`, `plan` -> `Plan`, anything else -> `Supervised`); the `[sandbox]` key keeps Claude vocabulary for back-compat.
- `[sandbox.capabilities]` rides claude-code `adapter_options` on the worktree backend (`build_agent_configuration`); the container backend builds its configuration from model and permission policy only — see `docs/sandbox.md`.
- `[sandbox.container] auth` / `auth_env` generalizing to the per-agent auth modes of § 13.2: future (`ClaudeAuth` unchanged today).

### 15.4 Container backend

`flywheel-container` keeps sandbox provisioning, teardown, networking, UID preflight, and auth mounts. Its invocation path is the claude-code adapter's CLI stream-json transport under `DockerExecHost`, built per run by the worker's `host_invoke_builder` (`crates/flywheel-worktree/src/daemon.rs`). The hang watchdog and per-message telemetry are live in containers because every parsed stream line becomes a normalized event.

### 15.5 Other retrofits

- Session-limit regexes live in both the claude-code adapter's `classify_fault` (`claude_code/faults.rs`) and core (`derive_session_limit_reset`, `crates/flywheel-core/src/faults.rs`, which also owns the clock-time reset derivation). Consolidating them is backlog.
- `CompletedRun.native_session_id` populating the `lifecycles.session_id` column: **not done** — the harness never persists it; needs a harness-side change (§ 16 Later).
- Re-expressing `docs/loop.md`'s detection table in normalized events with per-capability degradation: **not done** (§ 16 Later).

## 16. Delivery phases

The in-loop verification gate (`docs/loop.md`) applies to every phase that touches the loop.

1. **Foundation — SHIPPED.** Models, events, fold, adapter contract, hosts, scripted fake agents, contract test suites. No real-agent dependence in CI.
2. **claude-code adapter — SHIPPED.** CLI stream-json transport with the bidirectional control protocol (mid-turn control, context usage, approvals).
3. **Core bridge — SHIPPED.** Every invocation site routes through the runtime (§ 15.2); there is no legacy path.
4. **Container rebase — SHIPPED.** `DockerExecHost` + the claude-code CLI transport; the hang watchdog and per-event telemetry are live in containers; exec timeout and nonzero exits fold to structured failures (`error_type = "timeout"` / `"agent_exit"`).
5. **Second agent — SHIPPED (codex, headless JSONL).** The codex adapter drives `codex exec --json` under any execution host: thread/turn/item normalization (agent messages, reasoning, command executions with exit codes, file changes, MCP calls, web search, todo plans), sandbox-mode permission mapping (including a real `read-only`), `OPENAI_API_KEY` account-only stripping, and probe. The § 18 agent-swap acceptance test passes. Validated against a live binary (codex-cli 0.144.1, 2026-07-10): a full worker-stack dogfood run landed a codex-authored task through envelope verify, graders, and FF-merge. Remaining: the JSON-RPC app-server transport (approvals, mid-turn control); `classify_fault` deliberately recognizes nothing yet.

Later (consolidated backlog): the codex JSON-RPC app-server transport (§ 16 phase 5); the `loop.md` detection-table rewrite and session-limit regex consolidation (§ 15.5); persisting `native_session_id` onto `lifecycles.session_id` (§ 15.5); generalized auth propagation replacing `ClaudeAuth` and auth-policy wiring into `init` (§ 13.1, § 13.2); constructed-environment hardening (§ 13.3); the canonical MCP schema (§ 13.4); ACP transport (§ 11); managed-executable (`npx`) fallback and compatibility-range warnings (§ 10, § 14); option discovery surfaces; additional agents (Gemini/Qwen via ACP); a service layer via the interactivity program.

## 17. Testing strategy

- **Contract tests** every adapter must pass (`tests/contract_claude.rs`, `tests/contract_codex.rs`): start emits `session.started`; a prompt produces events; completion emits `session.finished`; cancel kills the tree; unknown native events surface as `raw`; stderr is captured; cwd and environment policy are respected; unsupported capabilities and transports fail explicitly.
- **Fake agents** — scripted test executables (`tests/common/mod.rs`: `fake_claude`, `fake_codex`) emulating JSONL output, delays, malformed JSON, native session ids, crashes, hangs, and control traffic. CI never depends on real vendor CLIs.
- **Fixtures** — normalizers, plan builders, and the fold are tested offline (`tests/claude_normalizer.rs`, `tests/codex_normalizer.rs`, `tests/claude_plan.rs`). Recording sanitized transcripts from real runs is the stronger future form.
- **Real integration tests** — conditional on installed infrastructure; never mandatory in CI. Docker-backed tests skip without a daemon; a conditional real-`claude` run is future work.

## 18. Acceptance criteria

An application (flywheel's loop) changes `agent_id` from `"claude-code"` to `"codex"` without changing orchestration logic. Differences appear only through capabilities, option discovery, event availability, and authentication assurance. **Holds:** proven by the branch-free agent-swap test (`agent_swap_changes_only_agent_id`, `crates/flywheel-agents/tests/contract_codex.rs`) and by live worker-stack dogfood runs of both agents landing verified tasks (2026-07-10).

Flywheel-specific (all hold):

1. `flywheel-agents` depends on no other flywheel crate.
2. The container backend runs through the same claude-code adapter as the worktree backend, with a live hang watchdog.
3. Every agent invocation site routes through the runtime.
4. The LOOP_STATUS envelope contract and its parser are untouched.

"""Repo-owned work policy: ``flywheel.toml``.

The policy file is the consumer repo's versioned contract with the
orchestrator — *where work comes from* and *what "runnable" means by
default* — so switching a project between a task directory and an issue
tracker is a committed config change, not a flywheel code change.

Format (TOML, stdlib ``tomllib``)::

    [source]
    kind = "directory"              # or "github" / "github_ci" / "github_review"
    tasks_dir = ".flywheel/tasks"   # directory kind (optional, default shown)

    # github kind:
    # repo = "owner/name"           # required
    # label = "flywheel"            # required: only issues with this label
    # done_action = "comment"       # or "close" (default "comment")

    # github_ci kind:
    # repo = "owner/name"           # required
    # failure_filter = "failure"    # gh run --status filter (optional default)

    # github_review kind:
    # repo = "owner/name"           # required: unresolved PR review threads.
    #                               # The grade is [defaults.graders] run
    #                               # out-of-band, never the thread's
    #                               # isResolved state (spec 00053, D-4).

    # Where runtime state lives (optional). CLI flags still win; without
    # these the built-in .flywheel/ defaults apply. sandbox_root also
    # accepts the tokens "@cache" (XDG cache dir keyed by repo identity)
    # and "@sibling" (<repo-parent>/<repo>.worktrees); relative paths
    # anchor at the repo root -- see resolve_sandbox_root.
    [paths]
    db = ".flywheel/flywheel.sqlite"
    sandbox_root = ".flywheel/worktrees"

    # Default grader policy: applied by tracker-backed sources to items
    # that do not declare their own graders. Directory tasks always carry
    # graders (the task schema requires them), so this section is only
    # meaningful for tracker kinds.
    [[defaults.graders]]
    type = "command"
    run = "uv run pytest"

    # Agent runtime settings (optional). The model id is an opaque string
    # passed verbatim to the SDK; flywheel does not maintain an allowlist.
    [agent]
    model = "claude-sonnet-4-5"

    # Store backend (optional). Absent section means sqlite, so every
    # pre-existing flywheel.toml keeps loading unchanged.
    [store]
    backend = "sqlite"              # or "postgres"
    # schema = "flywheel_ci"        # postgres only; optional

    # Execution policy (optional). mode is "local" (default) or
    # "distributed". capabilities is this worker's advertised capability set
    # (default empty): the scheduler offers this worker only items whose
    # required_capabilities is a subset of it. Distinct from
    # [sandbox.capabilities] (the agent's tool/skill/MCP surface).
    [execution]
    mode = "local"
    capabilities = ["gpu", "cuda"]

    # Sandbox provisioning (optional). setup runs (shell) inside every
    # newly created sandbox before the agent enters — dependency install,
    # codegen — so tasks never pay discovery cost for a bare worktree.
    # Reused parked sandboxes skip it (their environment survived).
    [sandbox]
    setup = "uv sync"

    # Landing policy (optional). strategy picks how DONE work leaves the
    # loop: "merge" FF-merges the task branch into the worker's base (full
    # autonomy, the default); "pr" pushes the branch and opens a pull
    # request with grader receipts in the body — review/CI own the merge.
    # protected_paths are glob patterns (PurePath.full_match semantics,
    # ** crosses directories) matched against every repo-relative path a
    # finished task's branch touches; any match refuses the landing and
    # parks the work. Protects the verification surface itself — grader
    # configs, CI, harness state — from being rewritten by the work it is
    # supposed to judge.
    [submit]
    strategy = "merge"              # or "pr"
    protected_paths = [".github/**", "flywheel.toml"]
    # remote = "origin"             # pr strategy: push target
    # pr_base = "main"              # pr strategy: PR base branch
    #                               # (default: the worker's base branch)

    # Phase-exit gate (optional). verify runs (shell) against the merged
    # phase base in repo_root once every task in a phase has landed; a
    # non-zero exit leaves the phase active. Unset means today's archival
    # (no gate).
    [phase]
    verify = "uv run pytest"

    # Held-out landing gate (optional). root is the directory of
    # operator-declared held-out grader registrations the execute-time gate
    # (spec 00050) reads, one ``<task_id>.json`` per gated task. The worker
    # resolves a relative root against the repo root. Unset means no gate is
    # activated: the worker builds no held-out source and landing is
    # byte-identical to today (spec 00051). There is deliberately no default
    # root -- a default would silently activate gating on upgrade.
    [held_out]
    root = ".flywheel/held-out"

The defaults keep flywheel's readiness gate mechanical without forcing
every ticket author to write graders: an issue with no graders and no
default policy is not runnable and never reaches the scheduler.
"""

from __future__ import annotations

import hashlib
import os
import subprocess
import tempfile
import tomllib
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

from flywheel_core.deadline_config import (
    DeadlineConfig,
    deadline_config_from_mapping,
)
from flywheel_core.loaders import TaskLoadError, load_graders
from flywheel_core.task import Grader

from flywheel_orchestrator._autopilot import (
    DEFAULT_LANDING,
    DEFAULT_TARGET_DEPTH,
    ScoreWeights,
)
from flywheel_orchestrator._claims import StopEventStore
from flywheel_orchestrator._github import GithubWorkSource
from flywheel_orchestrator._github_ci import GithubCiWorkSource
from flywheel_orchestrator._github_review import GithubReviewWorkSource
from flywheel_orchestrator._source_registry import SOURCES
from flywheel_orchestrator._sources import (
    DirectoryWorkSource,
    StopEventSink,
    WorkSource,
)

DEFAULT_POLICY_FILENAME = "flywheel.toml"

# Mirrors the names registered in ``_source_registry.SOURCES``; kept here for
# load-time policy validation (``_optional_source`` rejects an unknown kind
# before any work source is built). The registry owns construction dispatch.
_SOURCE_KINDS: tuple[str, ...] = (
    "directory",
    "github",
    "github_ci",
    "github_review",
)

_DONE_ACTIONS: tuple[str, ...] = ("comment", "close")

#: Default autopilot cycle interval (seconds) when [autopilot] omits one.
DEFAULT_AUTOPILOT_INTERVAL_SECONDS: float = 300.0

#: Landing postures autopilot work can take; FF-merge is the shipped default.
_AUTOPILOT_LANDINGS: tuple[str, ...] = ("merge", "pr")

_STORE_BACKENDS: tuple[str, ...] = ("sqlite", "postgres")

_EXECUTION_MODES: tuple[str, ...] = ("local", "distributed")

#: Default worker pool size when [worker] omits ``concurrency`` (single
#: serial worker — today's behavior byte-for-byte; spec 00060, D-1).
DEFAULT_WORKER_CONCURRENCY: int = 1

#: Default checkpoint-nudge threshold (seconds) when [worker] omits
#: ``checkpoint_nudge_seconds``. Default-on (300.0) so an iteration nearing its
#: AGENT_ITERATION deadline with no new commits is nudged to checkpoint; ``0``
#: disables the nudge. Mirrors :attr:`flywheel_core.harness.HarnessConfig.checkpoint_nudge_seconds`.
DEFAULT_CHECKPOINT_NUDGE_SECONDS: float = 300.0

#: Default ceiling (seconds) clamping a session-limit-driven pool-wide claim
#: pause when [worker] omits ``session_pause_ceiling_seconds`` (spec
#: session-limit-claim-pause). 21600.0 (6h) bounds how long a single derived
#: reset can hold the pool off claiming, so a far-future reset never yields an
#: unbounded pause; ``0`` disables pausing entirely (today's behavior
#: byte-for-byte).
DEFAULT_SESSION_PAUSE_CEILING_SECONDS: float = 21600.0

#: Default turn ceiling for the merge strategy's bounded conflict-resolution
#: session when [submit] omits ``recovery_agent_max_turns``. ``0`` disables the
#: rung entirely (a merge conflict parks exactly as merge-fallback does).
DEFAULT_RECOVERY_AGENT_MAX_TURNS: int = 30

#: Default wall-clock ceiling (seconds) for that session when [submit] omits
#: ``recovery_agent_max_wall_seconds``.
DEFAULT_RECOVERY_AGENT_MAX_WALL_SECONDS: float = 900.0


class PolicyError(ValueError):
    """Raised when a policy file is missing, unparseable, or invalid.

    The message always identifies the offending file (and key, where
    applicable) so the operator's fix is unambiguous.
    """


@dataclass(frozen=True, kw_only=True)
class SandboxExec:
    """``[sandbox.exec]`` — bash command isolation (SDK ``sandbox`` option).

    Wired into the host SDK agent options; ``fast``/``balanced`` disable it and
    ``hardened`` enables it.
    """

    enabled: bool = False
    auto_allow: bool = True


@dataclass(frozen=True, kw_only=True)
class SandboxCapabilities:
    """``[sandbox.capabilities]`` (+ ``.mcp``) — skills/tools/MCP surface.

    ``setting_sources`` uses an omit-on-unset ``None`` sentinel: the ``fast``
    baseline sets nothing so the SDK keeps deriving ``["user", "project"]``
    from ``skills="all"`` exactly as today. ``skills`` is ``"all"``/``"none"``
    or an explicit tuple of skill names.
    """

    skills: str | tuple[str, ...] = "all"
    allowed_tools: tuple[str, ...] = ()
    denied_tools: tuple[str, ...] = ()
    setting_sources: tuple[str, ...] | None = None
    mcp_servers: tuple[str, ...] = ()
    mcp_strict: bool = False


@dataclass(frozen=True, kw_only=True)
class SandboxNetwork:
    """``[sandbox.network]`` — network policy.

    Advisory under the worktree backend (enforces nothing); has real teeth
    only under ``[sandbox] backend = "container"``.
    """

    policy: str = "allow"
    allow_hosts: tuple[str, ...] = ()
    allow_unix_sockets: tuple[str, ...] = ()


@dataclass(frozen=True, kw_only=True)
class SandboxEnv:
    """``[sandbox.env]`` — credential/secret name allowlist (declaration half).

    ``passthrough`` mirrors the ``pass`` name allowlist; ``set_values`` mirrors
    the inline ``set`` literals. The ``fast`` baseline inherits the operator's
    full ambient environment (``inherit_home`` true, no explicit scoping).
    Resolved into the host SDK ``agent_env`` (``pass`` names from the operator
    environment, ``set`` literals winning) and into the command-grader
    environment by :func:`resolve_grader_env`, so the agent and the command
    graders that verify its work share the same environment (e.g. a build
    cache). The container backend bypasses the host SDK options, so these do
    not reach the in-container agent.
    """

    passthrough: tuple[str, ...] = ()
    set_values: dict[str, str] = field(default_factory=dict)
    inherit_home: bool = True


def resolve_grader_env(
    env: SandboxEnv, base_environ: Mapping[str, str] | None = None
) -> dict[str, str] | None:
    """Resolve ``[sandbox.env]`` into the full environment graders run with.

    The agent path resolves ``[sandbox.env]`` into *overrides* the SDK merges
    over the inherited environment. Command graders, by contrast, run via a
    subprocess whose ``env`` argument REPLACES the inherited environment, so
    this returns the FULL env (not just the overrides): the declared ``pass``
    names forwarded from ``base_environ`` (present-only) plus the inline ``set``
    literals (which win on collision), laid over the ambient environment when
    ``inherit_home`` is true, or scoped to only those values when it is false.

    Returns ``None`` when there is nothing to change (no overrides AND
    ``inherit_home``), so callers pass ``env=None`` and graders inherit exactly
    as before -- byte-identical to the behavior before ``[sandbox.env]`` was
    wired to graders. This is the grader-side companion of the agent-side
    resolution in ``_orchestrate._sandbox_agent_primitives``: both must see the
    same env so an agent build and the command grader that verifies it share,
    e.g., a Rust ``CARGO_TARGET_DIR`` / ``RUSTC_WRAPPER`` (build-cache parity).
    """
    environ = os.environ if base_environ is None else base_environ
    overrides = {n: environ[n] for n in env.passthrough if n in environ}
    overrides.update(env.set_values)
    if env.inherit_home:
        if not overrides:
            return None
        return {**environ, **overrides}
    return dict(overrides)


@dataclass(frozen=True, kw_only=True)
class SandboxLimits:
    """``[sandbox.limits]`` — resource/budget ceilings.

    ``max_cost_usd``/``max_tokens``/``wall_clock_seconds`` are enforced per-run
    cumulatively via ``HarnessConfig`` (``0`` = unenforced). ``max_turns``/
    ``max_retries``/``lease_seconds`` mirror today's hardcoded values and keep
    their existing CLI/default path (not lifted into ceiling enforcement).

    ``rubric_judge_max_turns`` is the per-judge-call turn budget forwarded onto
    :attr:`HarnessConfig.rubric_judge_max_turns`. ``None`` (an absent key) keeps
    the harness default (32); a present value must be a positive integer.
    """

    max_turns: int = 500
    max_retries: int = 1
    lease_seconds: int = 300
    wall_clock_seconds: int = 0
    max_cost_usd: float = 0.0
    max_tokens: int = 0
    rubric_judge_max_turns: int | None = None


@dataclass(frozen=True, kw_only=True)
class SandboxRetention:
    """``[sandbox.retention]`` — teardown/disposal policy.

    ``on_done``/``on_failure`` are threaded through the worktree submitter
    (done destroy, fail park by default). ``sweep_days`` stays on the CLI
    (``--worktree-retention-days``), not policy-threaded.
    """

    on_done: str = "destroy"
    on_failure: str = "park"
    sweep_days: int = 7


@dataclass(frozen=True, kw_only=True)
class SandboxContainer:
    """``[sandbox.container]`` — container backend config (spec 00045).

    Inert unless ``[sandbox] backend = "container"``, at which point ``image``
    is required. ``auth`` declares a MODE and ``auth_env`` the env var NAME the
    token is read from at run time — the value never lives in the policy file,
    mirroring ``[sandbox.env]``. ``model`` falls back to the worker's resolved
    agent model when empty. ``egress_network`` is the operator-provisioned
    network for ``[sandbox.network]`` ``allow_hosts``.
    """

    image: str = ""
    model: str = ""
    auth: str = "oauth"
    auth_env: str = ""
    exec_timeout: int = 1800
    egress_network: str = ""


@dataclass(frozen=True, kw_only=True)
class SandboxPolicy:
    """Resolved ``[sandbox.*]`` configuration: a preset baseline with sparse
    per-key repo overrides merged on top, frozen at load.

    The defaults of this dataclass (and its nested sub-dataclasses) *are* the
    ``fast`` preset and equal today's hardcoded construction verbatim, with
    omit-on-unset sentinels so downstream option construction stays
    byte-identical. Only ``fast`` is defined in this increment; selecting any
    other preset fails fast. Holds only primitives — no SDK types — to
    preserve the optional-SDK boundary.
    """

    preset: str = "fast"
    backend: str = "worktree"
    permission_mode: str = "bypassPermissions"
    exec: SandboxExec = field(default_factory=SandboxExec)
    capabilities: SandboxCapabilities = field(default_factory=SandboxCapabilities)
    network: SandboxNetwork = field(default_factory=SandboxNetwork)
    env: SandboxEnv = field(default_factory=SandboxEnv)
    limits: SandboxLimits = field(default_factory=SandboxLimits)
    retention: SandboxRetention = field(default_factory=SandboxRetention)
    container: SandboxContainer = field(default_factory=SandboxContainer)


# Named presets are code-owned frozen constants (factor V: build-time).
# ``fast``'s values are the dataclass defaults above (== today). ``balanced``
# adds strict MCP gating (no implicit servers) while keeping full coding
# capability and autonomy. ``hardened`` further locks the tool allowlist to a
# minimal coding set, scopes settings to the project, and enables bash exec
# isolation. Every preset keeps ``permission_mode == "bypassPermissions"``;
# deferred aspects (env/limits/network/retention) stay at ``fast`` so a preset
# never advertises a guarantee it cannot enforce.
_SANDBOX_PRESETS: dict[str, SandboxPolicy] = {
    "fast": SandboxPolicy(preset="fast"),
    "balanced": SandboxPolicy(
        preset="balanced",
        capabilities=SandboxCapabilities(mcp_strict=True),
    ),
    "hardened": SandboxPolicy(
        preset="hardened",
        capabilities=SandboxCapabilities(
            allowed_tools=("Bash", "Edit", "Glob", "Grep", "Read", "Write"),
            setting_sources=("project",),
            mcp_strict=True,
        ),
        exec=SandboxExec(enabled=True),
    ),
}

_NETWORK_POLICIES: tuple[str, ...] = ("allow", "deny")
_CONTAINER_AUTH_MODES: tuple[str, ...] = ("oauth", "session", "api_key", "none")
_RETENTION_ON_DONE: tuple[str, ...] = ("destroy", "preserve")
_RETENTION_ON_FAILURE: tuple[str, ...] = ("park", "destroy")

#: The closed risk-tier vocabulary of ``[[submit.tiers]]`` (spec 00080):
#: tier 0 lands direct (merge), tier 1 lands on the phase integration branch
#: (phase), tier 2 goes to a PR requiring human approval (pr). A file matching
#: no rule defaults to tier 1 (D-3) so a newly invented path is never a silent
#: direct-merge lane; the tier-to-strategy routing itself is worker-side.
SUBMIT_TIER_LEVELS: tuple[int, ...] = (0, 1, 2)
SUBMIT_TIER_DEFAULT: int = 1


@dataclass(frozen=True, kw_only=True)
class SubmitTierRule:
    """One ``[[submit.tiers]]`` path rule (spec 00080).

    ``paths`` are glob patterns over a DONE branch's changed files
    (``PurePath.full_match`` semantics, ``**`` crosses directories -- the same
    vocabulary as ``[submit] protected_paths``); ``tier`` is the risk tier a
    matching file classifies at, one of :data:`SUBMIT_TIER_LEVELS`. The
    classifier is worker-side and highest-wins: a file takes the highest tier
    of any matching rule, the highest file tier wins the diff, and an unmatched
    file classifies at :data:`SUBMIT_TIER_DEFAULT` (D-3). Tiers choose among
    legitimate landing routes; ``protected_paths`` remains the distinct,
    absolute refuse-list evaluated independently and first (D-1).
    """

    tier: int
    paths: tuple[str, ...]


@dataclass(frozen=True, kw_only=True)
class WorkPolicy:
    """Parsed, validated ``flywheel.toml`` contents.

    ``tasks_dir`` is populated for ``kind = "directory"``;
    ``github_repo``/``github_label``/``github_done_action`` for
    ``kind = "github"``; ``github_ci_repo``/``github_ci_failure_filter`` for
    ``kind = "github_ci"``; ``github_review_repo`` for
    ``kind = "github_review"``. ``default_graders`` is empty when the file
    declares none. ``db_path``/``sandbox_root`` mirror the optional
    ``[paths]`` table and are ``None`` when unset (the CLI then falls
    back to its built-in defaults). ``sandbox_root`` is stored verbatim --
    a path or an ``@cache``/``@sibling`` token -- and is turned into the
    absolute worktree root by :func:`resolve_sandbox_root` at use time. ``model`` mirrors the optional
    ``[agent] model`` key -- an opaque, repo-pinned model id passed
    verbatim to the SDK; ``None`` when unset (the worker then falls back
    to its CLI flag / built-in default). ``store_backend``/``store_schema``
    mirror the optional ``[store]`` table; an absent section means sqlite
    with no schema, so every pre-existing policy file keeps loading
    unchanged. ``protected_paths`` mirrors the optional
    ``[submit] protected_paths`` list; empty when unset (no merge-time
    path gate). ``submit_tiers`` mirrors the optional ``[[submit.tiers]]``
    array of path rules (spec 00080); empty when unset, which keeps landing
    identical to the configured ``submit_strategy`` with no classifier in
    the path. When non-empty the worker routes each DONE task's landing by
    the highest tier its changed files classify at (0 merge / 1 phase /
    2 pr), and ``submit_strategy`` no longer selects the route.
    ``submit_strategy``/``submit_remote``/``submit_pr_base``/``submit_base``
    mirror the rest of the optional ``[submit]`` table; an absent table
    means the historical merge landing. ``execution_mode`` mirrors the
    optional ``[execution] mode`` key (``"local"`` default, or
    ``"distributed"``); it is a pure load-time validation assertion that
    gates no runtime scheduling/claim/lease behavior, so an absent
    ``[execution]`` table resolves to ``local`` with no behavior change.
    ``execution_capabilities`` mirrors the optional ``[execution]
    capabilities`` list -- this worker's advertised capability set (spec
    00049, decision D-2), distinct from ``[sandbox.capabilities]`` (the
    agent's tool/skill/MCP surface). The scheduler offers this worker only
    items whose ``required_capabilities`` is a subset of it; empty (the
    default, an absent key) preserves today's behavior -- every existing
    zero-requirement item is selectable.
    ``worker_concurrency`` mirrors the optional ``[worker] concurrency``
    key -- the worker pool size a single ``flywheel worker`` invocation
    drains the queue with (spec 00060, D-1). It defaults to ``1`` (an
    absent ``[worker]`` table means single serial worker, byte-for-byte
    today's behavior) and is overridden per-run by ``--concurrency``. The
    ``< 1`` range check is deliberately NOT enforced at load time: because
    the flag overrides the config, a sub-1 value is only an error once it
    is the *resolved* pool size (D-4), so the worker validates the resolved
    value, not this field.
    ``worker_checkpoint_nudge_seconds`` mirrors the optional ``[worker]
    checkpoint_nudge_seconds`` key -- the remaining-wall-time threshold the
    worker threads into the harness's checkpoint-nudge seam (the
    :class:`~flywheel_core.harness.HarnessConfig` knob of the same name). It
    defaults to ``300.0`` (default-on: an absent ``[worker]`` table still
    nudges an iteration nearing its ``AGENT_ITERATION`` deadline on a branch
    with no new commits); ``0`` disables the nudge. The concrete git progress
    probe is supplied by the worker, not this policy.
    ``submit_base`` is the explicit
    landing/phase-base branch; ``None`` falls back to the checked-out
    branch (back-compat), mirroring ``submit_pr_base``. ``sandbox_setup``
    mirrors the optional ``[sandbox] setup`` command; ``None`` when unset
    (new sandboxes are used bare). ``phase_verify`` mirrors the optional
    ``[phase] verify`` command run against the merged phase base before a
    phase archives; ``None`` when unset (no phase-exit gate, today's
    archival behavior). ``held_out_root`` mirrors the optional
    ``[held_out] root`` key -- the directory of operator-declared held-out
    grader registrations the execute-time landing gate (spec 00050) reads,
    resolved relative to the repo root by the worker; ``None`` when unset so
    the worker builds no held-out source and landing stays byte-identical
    (spec 00051, criterion #2, decision D-3). A default that silently
    activates gating on upgrade is deliberately absent.
    ``deadlines`` mirrors the optional top-level ``[deadlines]`` table (spec
    00066): the default-on, operator-overridable wall-clock ceilings resolved
    via :func:`flywheel_core.deadline_config.deadline_config_from_mapping`. An
    absent table yields a default :class:`DeadlineConfig` (finite, non-null
    ceilings), byte-identical to today; a key set to ``0`` opts that class out
    (unbounded).
    """

    source_kind: str
    tasks_dir: Path | None = None
    github_repo: str | None = None
    github_label: str | None = None
    github_done_action: str = "comment"
    github_ci_repo: str | None = None
    github_ci_failure_filter: str = "failure"
    github_review_repo: str | None = None
    default_graders: tuple[Grader, ...] = ()
    db_path: Path | None = None
    sandbox_root: Path | None = None
    model: str | None = None
    store_backend: str = "sqlite"
    store_schema: str | None = None
    execution_mode: str = "local"
    execution_capabilities: frozenset[str] = frozenset()
    worker_concurrency: int = DEFAULT_WORKER_CONCURRENCY
    worker_checkpoint_nudge_seconds: float = DEFAULT_CHECKPOINT_NUDGE_SECONDS
    worker_session_pause_ceiling_seconds: float = (
        DEFAULT_SESSION_PAUSE_CEILING_SECONDS
    )
    protected_paths: tuple[str, ...] = ()
    submit_tiers: tuple[SubmitTierRule, ...] = ()
    submit_strategy: str = "merge"
    submit_remote: str = "origin"
    submit_pr_base: str | None = None
    submit_base: str | None = None
    submit_verify: str | None = None
    submit_recovery_agent_max_turns: int = DEFAULT_RECOVERY_AGENT_MAX_TURNS
    submit_recovery_agent_max_wall_seconds: float = (
        DEFAULT_RECOVERY_AGENT_MAX_WALL_SECONDS
    )
    sandbox_setup: str | None = None
    phase_verify: str | None = None
    held_out_root: Path | None = None
    sandbox: SandboxPolicy = field(default_factory=SandboxPolicy)
    deadlines: DeadlineConfig = field(default_factory=DeadlineConfig)
    autopilot_target_depth: int = DEFAULT_TARGET_DEPTH
    autopilot_landing: str = DEFAULT_LANDING
    autopilot_interval_seconds: float = DEFAULT_AUTOPILOT_INTERVAL_SECONDS
    autopilot_weights: ScoreWeights | None = None


def load_policy(path: Path) -> WorkPolicy:
    """Parse and validate the policy file at ``path``."""
    try:
        text = path.read_bytes()
    except OSError as exc:
        raise PolicyError(f"{path}: cannot read policy file: {exc}") from exc
    try:
        data = tomllib.loads(text.decode("utf-8"))
    except (tomllib.TOMLDecodeError, UnicodeDecodeError) as exc:
        raise PolicyError(f"{path}: invalid TOML: {exc}") from exc

    source = data.get("source")
    if not isinstance(source, dict):
        raise PolicyError(f"{path}: missing required [source] table")
    kind = source.get("kind")
    if kind not in _SOURCE_KINDS:
        raise PolicyError(
            f"{path}: source.kind must be one of {_SOURCE_KINDS}, "
            f"got {kind!r}"
        )

    defaults = data.get("defaults") or {}
    if not isinstance(defaults, dict):
        raise PolicyError(f"{path}: [defaults] must be a table")
    raw_graders = defaults.get("graders", [])
    try:
        default_graders = tuple(
            load_graders(list(raw_graders), source=f"{path}: defaults")
        )
    except TaskLoadError as exc:
        raise PolicyError(str(exc)) from exc

    paths = data.get("paths") or {}
    if not isinstance(paths, dict):
        raise PolicyError(f"{path}: [paths] must be a table")
    db_path = _optional_path(paths, "db", policy_file=path)
    sandbox_root = _optional_path(paths, "sandbox_root", policy_file=path)

    agent = data.get("agent") or {}
    if not isinstance(agent, dict):
        raise PolicyError(f"{path}: [agent] must be a table")
    model = _optional_agent_model(agent, policy_file=path)

    store = data.get("store") or {}
    if not isinstance(store, dict):
        raise PolicyError(f"{path}: [store] must be a table")
    store_backend, store_schema = _optional_store(store, policy_file=path)

    execution = data.get("execution") or {}
    if not isinstance(execution, dict):
        raise PolicyError(f"{path}: [execution] must be a table")
    execution_mode = _optional_execution_mode(execution, policy_file=path)
    execution_capabilities = _optional_execution_capabilities(
        execution, policy_file=path
    )
    if execution_mode == "distributed" and store_backend != "postgres":
        raise PolicyError(
            f"{path}: execution.mode = 'distributed' requires "
            f"store.backend = 'postgres', got store.backend = "
            f"{store_backend!r}"
        )

    submit = data.get("submit") or {}
    if not isinstance(submit, dict):
        raise PolicyError(f"{path}: [submit] must be a table")
    protected_paths = _optional_protected_paths(submit, policy_file=path)
    submit_tiers = _optional_submit_tiers(submit, policy_file=path)
    (
        submit_strategy,
        submit_remote,
        submit_pr_base,
        submit_base,
    ) = _optional_submit_strategy(submit, policy_file=path)
    submit_verify = _optional_submit_verify(submit, policy_file=path)
    (
        submit_recovery_agent_max_turns,
        submit_recovery_agent_max_wall_seconds,
    ) = _optional_submit_recovery_agent(submit, policy_file=path)

    # Team-mode guard (spec 00081, criterion 2): the merge lock is an
    # fcntl.flock on a local path -- advisory, kernel-local, single-machine
    # by construction. Distributed execution with a landing that serializes
    # through it (merge or phase directly, or [[submit.tiers]] whose tiers
    # 0/1 route through them) would let two machines silently race the base
    # branch, so it is a load-time configuration error. The distributed
    # landing route is 'pr': GitHub's merge queue and branch protection are
    # the distributed merge lock (docs/team-mode.md).
    if execution_mode == "distributed":
        if submit_strategy in LOCAL_LOCK_STRATEGIES:
            raise PolicyError(
                f"{path}: execution.mode = 'distributed' cannot use "
                f"submit.strategy = {submit_strategy!r}: the "
                f"{submit_strategy} strategy serializes landings through "
                f"the single-machine merge lock (.flywheel/.merge.lock), "
                f"which cannot span machines. Use submit.strategy = 'pr' "
                f"with a merge queue (docs/team-mode.md), or "
                f"execution.mode = 'local'"
            )
        if submit_tiers:
            raise PolicyError(
                f"{path}: execution.mode = 'distributed' cannot use "
                f"[[submit.tiers]]: tier routing lands tiers 0 and 1 "
                f"through the single-machine merge lock "
                f"(.flywheel/.merge.lock), which cannot span machines. Use "
                f"submit.strategy = 'pr' with a merge queue "
                f"(docs/team-mode.md), or execution.mode = 'local'"
            )

    sandbox = data.get("sandbox") or {}
    if not isinstance(sandbox, dict):
        raise PolicyError(f"{path}: [sandbox] must be a table")
    sandbox_setup = _optional_sandbox_setup(sandbox, policy_file=path)
    sandbox_policy = _optional_sandbox_policy(sandbox, policy_file=path)

    phase = data.get("phase") or {}
    if not isinstance(phase, dict):
        raise PolicyError(f"{path}: [phase] must be a table")
    phase_verify = _optional_phase_verify(phase, policy_file=path)

    held_out = data.get("held_out") or {}
    if not isinstance(held_out, dict):
        raise PolicyError(f"{path}: [held_out] must be a table")
    held_out_root = _optional_held_out_root(held_out, policy_file=path)

    deadlines = _optional_deadlines(data.get("deadlines"), policy_file=path)

    autopilot = data.get("autopilot") or {}
    if not isinstance(autopilot, dict):
        raise PolicyError(f"{path}: [autopilot] must be a table")
    (
        autopilot_target_depth,
        autopilot_landing,
        autopilot_interval_seconds,
        autopilot_weights,
    ) = _optional_autopilot(autopilot, policy_file=path)

    worker = data.get("worker") or {}
    if not isinstance(worker, dict):
        raise PolicyError(f"{path}: [worker] must be a table")
    (
        worker_concurrency,
        worker_checkpoint_nudge_seconds,
        worker_session_pause_ceiling_seconds,
    ) = _optional_worker(worker, policy_file=path)

    if kind == "directory":
        raw_dir = source.get("tasks_dir")
        if raw_dir is not None and not isinstance(raw_dir, str):
            raise PolicyError(f"{path}: source.tasks_dir must be a string")
        # The historical default lives in _workflow; duplicated here as a
        # late import to keep this module's import graph minimal.
        from flywheel_orchestrator._workflow import DEFAULT_TASKS_DIR

        return WorkPolicy(
            source_kind="directory",
            tasks_dir=Path(raw_dir) if raw_dir else DEFAULT_TASKS_DIR,
            default_graders=default_graders,
            db_path=db_path,
            sandbox_root=sandbox_root,
            model=model,
            store_backend=store_backend,
            store_schema=store_schema,
            execution_mode=execution_mode,
            execution_capabilities=execution_capabilities,
            worker_concurrency=worker_concurrency,
            worker_checkpoint_nudge_seconds=worker_checkpoint_nudge_seconds,
            worker_session_pause_ceiling_seconds=(
                worker_session_pause_ceiling_seconds
            ),
            protected_paths=protected_paths,
            submit_tiers=submit_tiers,
            submit_strategy=submit_strategy,
            submit_remote=submit_remote,
            submit_pr_base=submit_pr_base,
            submit_base=submit_base,
            submit_verify=submit_verify,
            submit_recovery_agent_max_turns=submit_recovery_agent_max_turns,
            submit_recovery_agent_max_wall_seconds=(
                submit_recovery_agent_max_wall_seconds
            ),
            sandbox_setup=sandbox_setup,
            phase_verify=phase_verify,
            held_out_root=held_out_root,
            sandbox=sandbox_policy,
            deadlines=deadlines,
            autopilot_target_depth=autopilot_target_depth,
            autopilot_landing=autopilot_landing,
            autopilot_interval_seconds=autopilot_interval_seconds,
            autopilot_weights=autopilot_weights,
        )

    if kind == "github_ci":
        ci_repo = source.get("repo")
        if not isinstance(ci_repo, str) or not ci_repo:
            raise PolicyError(
                f"{path}: source.repo is required for kind = \"github_ci\""
            )
        failure_filter = source.get("failure_filter", "failure")
        if not isinstance(failure_filter, str) or not failure_filter:
            raise PolicyError(
                f"{path}: source.failure_filter must be a non-empty string"
            )
        return WorkPolicy(
            source_kind="github_ci",
            github_ci_repo=ci_repo,
            github_ci_failure_filter=failure_filter,
            default_graders=default_graders,
            db_path=db_path,
            sandbox_root=sandbox_root,
            model=model,
            store_backend=store_backend,
            store_schema=store_schema,
            execution_mode=execution_mode,
            execution_capabilities=execution_capabilities,
            worker_concurrency=worker_concurrency,
            worker_checkpoint_nudge_seconds=worker_checkpoint_nudge_seconds,
            worker_session_pause_ceiling_seconds=(
                worker_session_pause_ceiling_seconds
            ),
            protected_paths=protected_paths,
            submit_tiers=submit_tiers,
            submit_strategy=submit_strategy,
            submit_remote=submit_remote,
            submit_pr_base=submit_pr_base,
            submit_base=submit_base,
            submit_verify=submit_verify,
            submit_recovery_agent_max_turns=submit_recovery_agent_max_turns,
            submit_recovery_agent_max_wall_seconds=(
                submit_recovery_agent_max_wall_seconds
            ),
            sandbox_setup=sandbox_setup,
            phase_verify=phase_verify,
            held_out_root=held_out_root,
            sandbox=sandbox_policy,
            deadlines=deadlines,
            autopilot_target_depth=autopilot_target_depth,
            autopilot_landing=autopilot_landing,
            autopilot_interval_seconds=autopilot_interval_seconds,
            autopilot_weights=autopilot_weights,
        )

    if kind == "github_review":
        review_repo = source.get("repo")
        if not isinstance(review_repo, str) or not review_repo:
            raise PolicyError(
                f"{path}: source.repo is required for kind = \"github_review\""
            )
        return WorkPolicy(
            source_kind="github_review",
            github_review_repo=review_repo,
            default_graders=default_graders,
            db_path=db_path,
            sandbox_root=sandbox_root,
            model=model,
            store_backend=store_backend,
            store_schema=store_schema,
            execution_mode=execution_mode,
            execution_capabilities=execution_capabilities,
            worker_concurrency=worker_concurrency,
            worker_checkpoint_nudge_seconds=worker_checkpoint_nudge_seconds,
            worker_session_pause_ceiling_seconds=(
                worker_session_pause_ceiling_seconds
            ),
            protected_paths=protected_paths,
            submit_tiers=submit_tiers,
            submit_strategy=submit_strategy,
            submit_remote=submit_remote,
            submit_pr_base=submit_pr_base,
            submit_base=submit_base,
            submit_verify=submit_verify,
            submit_recovery_agent_max_turns=submit_recovery_agent_max_turns,
            submit_recovery_agent_max_wall_seconds=(
                submit_recovery_agent_max_wall_seconds
            ),
            sandbox_setup=sandbox_setup,
            phase_verify=phase_verify,
            held_out_root=held_out_root,
            sandbox=sandbox_policy,
            deadlines=deadlines,
            autopilot_target_depth=autopilot_target_depth,
            autopilot_landing=autopilot_landing,
            autopilot_interval_seconds=autopilot_interval_seconds,
            autopilot_weights=autopilot_weights,
        )

    repo = source.get("repo")
    label = source.get("label")
    if not isinstance(repo, str) or not repo:
        raise PolicyError(
            f"{path}: source.repo is required for kind = \"github\""
        )
    if not isinstance(label, str) or not label:
        raise PolicyError(
            f"{path}: source.label is required for kind = \"github\""
        )
    done_action = source.get("done_action", "comment")
    if done_action not in _DONE_ACTIONS:
        raise PolicyError(
            f"{path}: source.done_action must be one of {_DONE_ACTIONS}, "
            f"got {done_action!r}"
        )
    return WorkPolicy(
        source_kind="github",
        github_repo=repo,
        github_label=label,
        github_done_action=done_action,
        default_graders=default_graders,
        db_path=db_path,
        sandbox_root=sandbox_root,
        model=model,
        store_backend=store_backend,
        store_schema=store_schema,
        execution_mode=execution_mode,
        execution_capabilities=execution_capabilities,
        worker_concurrency=worker_concurrency,
        worker_checkpoint_nudge_seconds=worker_checkpoint_nudge_seconds,
        worker_session_pause_ceiling_seconds=(
            worker_session_pause_ceiling_seconds
        ),
        protected_paths=protected_paths,
        submit_strategy=submit_strategy,
        submit_remote=submit_remote,
        submit_pr_base=submit_pr_base,
        submit_base=submit_base,
        submit_verify=submit_verify,
        submit_recovery_agent_max_turns=submit_recovery_agent_max_turns,
        submit_recovery_agent_max_wall_seconds=(
            submit_recovery_agent_max_wall_seconds
        ),
        sandbox_setup=sandbox_setup,
        phase_verify=phase_verify,
        held_out_root=held_out_root,
        sandbox=sandbox_policy,
        deadlines=deadlines,
        autopilot_target_depth=autopilot_target_depth,
        autopilot_landing=autopilot_landing,
        autopilot_interval_seconds=autopilot_interval_seconds,
        autopilot_weights=autopilot_weights,
    )


def _optional_path(
    table: dict, key: str, *, policy_file: Path
) -> Path | None:
    value = table.get(key)
    if value is None:
        return None
    if not isinstance(value, str) or not value:
        raise PolicyError(
            f"{policy_file}: paths.{key} must be a non-empty string"
        )
    return Path(value)


_SANDBOX_ROOT_CACHE_TOKEN = "@cache"
_SANDBOX_ROOT_SIBLING_TOKEN = "@sibling"


def resolve_sandbox_root(
    configured: str | Path | None,
    *,
    repo_root: Path,
) -> Path:
    """Resolve ``[paths] sandbox_root`` to the absolute worktree root.

    ``None`` (unset) keeps the built-in nested default
    ``<repo_root>/.flywheel/worktrees``. A relative path anchors at
    ``repo_root`` -- never the process cwd -- so the same policy file means
    the same location no matter where the CLI is invoked from. An absolute
    path is used verbatim. Two tokens opt into out-of-tree layouts:

    * ``@cache`` -- ``<cache-base>/flywheel/<repo-name>-<id>/worktrees``,
      where ``<cache-base>`` is the first writable of ``$XDG_CACHE_HOME``,
      ``~/.cache``, and the platform tmpdir, and ``<id>`` keys the repo by
      the realpath of its git common dir (two clones never collide; a
      linked-worktree checkout maps to its main repo).
    * ``@sibling`` -- ``<repo-parent>/<repo-name>.worktrees``, refused with
      :class:`PolicyError` when the parent directory is not writable
      (read-only CI mounts), rather than failing later mid-task.
    """
    if configured is None:
        return (repo_root / ".flywheel" / "worktrees").resolve()
    raw = str(configured)
    if raw == _SANDBOX_ROOT_CACHE_TOKEN:
        return _cache_sandbox_root(repo_root)
    if raw == _SANDBOX_ROOT_SIBLING_TOKEN:
        return _sibling_sandbox_root(repo_root)
    if raw.startswith("@"):
        raise PolicyError(
            f"paths.sandbox_root: unknown token {raw!r} "
            f"(supported: {_SANDBOX_ROOT_CACHE_TOKEN}, "
            f"{_SANDBOX_ROOT_SIBLING_TOKEN})"
        )
    path = Path(raw)
    if path.is_absolute():
        return path
    return (repo_root / path).resolve()


def _sibling_sandbox_root(repo_root: Path) -> Path:
    root = repo_root.resolve()
    parent = root.parent
    if parent == root:
        raise PolicyError(
            "paths.sandbox_root: @sibling needs a parent directory, but "
            f"{root} is the filesystem root"
        )
    if not os.access(parent, os.W_OK | os.X_OK):
        raise PolicyError(
            "paths.sandbox_root: @sibling requires a writable parent "
            f"directory, but {parent} is not writable (read-only mount?); "
            "use @cache or a literal path instead"
        )
    return parent / f"{root.name}.worktrees"


def _cache_sandbox_root(repo_root: Path) -> Path:
    base = _first_writable_cache_base()
    identity = _repo_identity(repo_root)
    digest = hashlib.sha256(str(identity).encode("utf-8")).hexdigest()[:12]
    name = repo_root.resolve().name or "repo"
    return base / "flywheel" / f"{name}-{digest}" / "worktrees"


def _first_writable_cache_base() -> Path:
    candidates: list[Path] = []
    xdg = os.environ.get("XDG_CACHE_HOME")
    if xdg:
        candidates.append(Path(xdg))
    try:
        candidates.append(Path.home() / ".cache")
    except RuntimeError:
        pass
    candidates.append(Path(tempfile.gettempdir()))
    for candidate in candidates:
        try:
            candidate.mkdir(parents=True, exist_ok=True)
        except OSError:
            continue
        if os.access(candidate, os.W_OK | os.X_OK):
            return candidate
    raise PolicyError(
        "paths.sandbox_root: @cache found no writable cache directory "
        f"(tried {', '.join(str(c) for c in candidates)})"
    )


def _repo_identity(repo_root: Path) -> Path:
    """The path that names this repo for ``@cache`` keying.

    The realpath of the git common dir, so every checkout of one repo --
    including linked worktrees -- shares a cache dir, while two clones at
    different paths never collide. Outside a git repo (the orchestrate
    plain-dir path) the realpath of ``repo_root`` itself is the identity.
    """
    proc = subprocess.run(
        ["git", "-C", str(repo_root), "rev-parse", "--git-common-dir"],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode == 0 and proc.stdout.strip():
        common = Path(proc.stdout.strip())
        if not common.is_absolute():
            common = repo_root / common
        return Path(os.path.realpath(common))
    return Path(os.path.realpath(repo_root))


def _optional_agent_model(
    table: dict, *, policy_file: Path
) -> str | None:
    """Validate and return the optional ``agent.model`` string.

    Returns ``None`` when the key is absent so an unconfigured policy
    leaves :attr:`WorkPolicy.model` at its default. A non-string value, or
    a string that is empty or whitespace-only, raises :class:`PolicyError`
    so a typo never silently degrades into "no model pinned." No
    allowlist is enforced -- the value is an opaque identifier passed
    through verbatim to the SDK.
    """
    value = table.get("model")
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise PolicyError(
            f"{policy_file}: agent.model must be a non-empty string"
        )
    return value


def _optional_store(
    table: dict, *, policy_file: Path
) -> tuple[str, str | None]:
    """Validate and return the optional ``[store]`` table contents.

    Returns ``("sqlite", None)`` when the section (or the ``backend``
    key) is absent so every pre-existing policy file keeps loading
    unchanged. An unknown backend, or a ``schema`` that is not a
    non-empty string, raises :class:`PolicyError` so a typo never
    silently degrades into the sqlite default.
    """
    backend = table.get("backend", "sqlite")
    if backend not in _STORE_BACKENDS:
        raise PolicyError(
            f"{policy_file}: store.backend must be one of "
            f"{_STORE_BACKENDS}, got {backend!r}"
        )
    schema = table.get("schema")
    if schema is None:
        return backend, None
    if not isinstance(schema, str) or not schema.strip():
        raise PolicyError(
            f"{policy_file}: store.schema must be a non-empty string"
        )
    return backend, schema


def _optional_execution_mode(
    table: dict, *, policy_file: Path
) -> str:
    """Validate and return the optional ``[execution] mode`` value.

    Returns ``"local"`` when the section (or the ``mode`` key) is absent so
    every pre-existing policy file keeps loading unchanged. A present-but-
    unrecognized mode raises :class:`PolicyError` naming ``execution.mode``
    and the offending value, mirroring the strict-validate convention of
    ``store.backend``/``source.kind`` so a typo never silently coerces to
    ``local``. Unknown *keys* under ``[execution]`` are ignored
    (forward-compat), matching the ``_optional_*`` pattern.
    """
    mode = table.get("mode", "local")
    if mode not in _EXECUTION_MODES:
        raise PolicyError(
            f"{policy_file}: execution.mode must be one of "
            f"{_EXECUTION_MODES}, got {mode!r}"
        )
    return mode


def _optional_execution_capabilities(
    table: dict, *, policy_file: Path
) -> frozenset[str]:
    """Validate and return the optional ``[execution] capabilities`` set.

    This worker's advertised capability set (spec 00049, decision D-2):
    the scheduler offers this worker only items whose
    ``required_capabilities`` is a subset of it. Returns ``frozenset()``
    when the section (or the ``capabilities`` key) is absent so every
    pre-existing policy file keeps loading unchanged and every existing
    zero-requirement item stays selectable. A non-list value, or any entry
    that is not a non-empty string, raises :class:`PolicyError` so a typo
    never silently degrades into "no capabilities advertised."
    """
    value = table.get("capabilities")
    if value is None:
        return frozenset()
    if not isinstance(value, list) or not all(
        isinstance(item, str) and item.strip() for item in value
    ):
        raise PolicyError(
            f"{policy_file}: execution.capabilities must be a list of "
            f"non-empty strings"
        )
    return frozenset(value)


def _optional_worker(
    table: dict, *, policy_file: Path
) -> tuple[int, float, float]:
    """Validate the optional ``[worker]`` table, returning
    ``(concurrency, checkpoint_nudge_seconds, session_pause_ceiling_seconds)``.

    ``concurrency`` returns ``1`` (the :data:`DEFAULT_WORKER_CONCURRENCY`
    single-serial-worker default) when the section (or the ``concurrency`` key)
    is absent so every pre-existing policy file keeps today's behavior
    byte-for-byte. A non-integer value (including a TOML boolean) raises
    :class:`PolicyError` naming ``worker.concurrency`` so a typo never silently
    degrades the pool size.

    The ``< 1`` range check is deliberately NOT enforced here: ``--concurrency``
    overrides this config value (spec 00060, D-1), so a sub-1 config is only an
    error once it is the *resolved* pool size (decision D-4, validated by the
    worker). A config of ``0`` with ``--concurrency 3`` is valid, so rejecting
    ``0`` at load time would be wrong.

    ``checkpoint_nudge_seconds`` returns :data:`DEFAULT_CHECKPOINT_NUDGE_SECONDS`
    (``300.0``, default-on) when the section or the key is absent -- the retro
    loss came from this nudge not existing, so it stays on by default. ``0``
    disables the nudge; a non-number raises :class:`PolicyError` naming
    ``worker.checkpoint_nudge_seconds``.

    ``session_pause_ceiling_seconds`` returns
    :data:`DEFAULT_SESSION_PAUSE_CEILING_SECONDS` (``21600.0`` = 6h, default-on)
    when the section or the key is absent -- so a run that surfaces a
    session-limit reset pauses pool-wide claiming until the reset, clamped to at
    most this many seconds ahead. ``0`` disables pausing entirely (today's
    behavior byte-for-byte); a non-number raises :class:`PolicyError` naming
    ``worker.session_pause_ceiling_seconds``.
    """
    concurrency = _override_int(
        table,
        "concurrency",
        DEFAULT_WORKER_CONCURRENCY,
        path="worker.concurrency",
        policy_file=policy_file,
    )
    checkpoint_nudge_seconds = _override_float(
        table,
        "checkpoint_nudge_seconds",
        DEFAULT_CHECKPOINT_NUDGE_SECONDS,
        path="worker.checkpoint_nudge_seconds",
        policy_file=policy_file,
    )
    session_pause_ceiling_seconds = _override_float(
        table,
        "session_pause_ceiling_seconds",
        DEFAULT_SESSION_PAUSE_CEILING_SECONDS,
        path="worker.session_pause_ceiling_seconds",
        policy_file=policy_file,
    )
    return concurrency, checkpoint_nudge_seconds, session_pause_ceiling_seconds


def _optional_protected_paths(
    table: dict, *, policy_file: Path
) -> tuple[str, ...]:
    """Validate and return the optional ``submit.protected_paths`` list.

    Returns ``()`` when the key is absent so an unconfigured policy has no
    merge-time path gate. A non-list value, or any entry that is not a
    non-empty string, raises :class:`PolicyError` so a typo never silently
    degrades into "nothing protected."
    """
    value = table.get("protected_paths")
    if value is None:
        return ()
    if not isinstance(value, list) or not all(
        isinstance(item, str) and item.strip() for item in value
    ):
        raise PolicyError(
            f"{policy_file}: submit.protected_paths must be a list of "
            f"non-empty strings"
        )
    return tuple(value)


def _optional_submit_tiers(
    table: dict, *, policy_file: Path
) -> tuple[SubmitTierRule, ...]:
    """Validate and return the optional ``[[submit.tiers]]`` rules (spec 00080).

    Returns ``()`` when the key is absent (or an explicit empty array) so an
    unconfigured policy keeps the single ``submit.strategy`` landing with no
    classifier in the path. Each rule must be a table carrying exactly ``tier``
    (an integer in :data:`SUBMIT_TIER_LEVELS`) and ``paths`` (a non-empty list
    of non-empty glob strings, ``protected_paths`` vocabulary). Anything else
    -- a non-array value, a stray key, a bool-typed or out-of-range tier, a
    malformed paths list -- raises :class:`PolicyError` naming the offense, so
    a typo never silently reclassifies a route.
    """
    value = table.get("tiers")
    if value is None:
        return ()
    if not isinstance(value, list) or not all(
        isinstance(item, dict) for item in value
    ):
        raise PolicyError(
            f"{policy_file}: submit.tiers must be an array of tables "
            f"([[submit.tiers]] entries with 'tier' and 'paths' keys)"
        )
    rules: list[SubmitTierRule] = []
    for index, item in enumerate(value):
        unknown = set(item) - {"tier", "paths"}
        if unknown:
            raise PolicyError(
                f"{policy_file}: submit.tiers[{index}] has unknown "
                f"key(s) {sorted(unknown)}; each rule takes exactly "
                f"'tier' and 'paths'"
            )
        tier = item.get("tier")
        if (
            isinstance(tier, bool)
            or not isinstance(tier, int)
            or tier not in SUBMIT_TIER_LEVELS
        ):
            raise PolicyError(
                f"{policy_file}: submit.tiers[{index}].tier must be one "
                f"of {SUBMIT_TIER_LEVELS}, got {tier!r}"
            )
        paths = item.get("paths")
        if (
            not isinstance(paths, list)
            or not paths
            or not all(
                isinstance(entry, str) and entry.strip() for entry in paths
            )
        ):
            raise PolicyError(
                f"{policy_file}: submit.tiers[{index}].paths must be a "
                f"non-empty list of non-empty strings"
            )
        rules.append(SubmitTierRule(tier=tier, paths=tuple(paths)))
    return tuple(rules)


_SUBMIT_STRATEGIES: tuple[str, ...] = ("merge", "pr", "phase")

#: The strategies that serialize landings through the single-machine merge
#: lock (``fcntl.flock`` on ``.flywheel/.merge.lock`` -- advisory, kernel-
#: local, single-machine by construction). The shared invariant of the
#: distributed-mode guard in :func:`load_policy` and the
#: ``docs/team-mode.md`` strategy table (spec 00081): a future strategy
#: addition must update both together.
LOCAL_LOCK_STRATEGIES: tuple[str, ...] = ("merge", "phase")


def _optional_submit_strategy(
    table: dict, *, policy_file: Path
) -> tuple[str, str, str | None, str | None]:
    """Validate and return ``(strategy, remote, pr_base, base)`` from
    ``[submit]``.

    Absent keys mean the historical merge landing (``("merge", "origin",
    None, None)``) so every pre-existing policy file keeps loading
    unchanged. An unknown strategy, or a remote/pr_base/base that is not a
    non-empty string, raises :class:`PolicyError`. ``base`` is the
    explicit landing/phase-base branch; ``None`` falls back to the
    checked-out branch (back-compat), mirroring ``pr_base``.
    """
    strategy = table.get("strategy", "merge")
    if strategy not in _SUBMIT_STRATEGIES:
        raise PolicyError(
            f"{policy_file}: submit.strategy must be one of "
            f"{_SUBMIT_STRATEGIES}, got {strategy!r}"
        )
    remote = table.get("remote", "origin")
    if not isinstance(remote, str) or not remote.strip():
        raise PolicyError(
            f"{policy_file}: submit.remote must be a non-empty string"
        )
    pr_base = table.get("pr_base")
    if pr_base is not None and (
        not isinstance(pr_base, str) or not pr_base.strip()
    ):
        raise PolicyError(
            f"{policy_file}: submit.pr_base must be a non-empty string"
        )
    base = table.get("base")
    if base is not None and (
        not isinstance(base, str) or not base.strip()
    ):
        raise PolicyError(
            f"{policy_file}: submit.base must be a non-empty string"
        )
    return strategy, remote, pr_base, base


def _optional_submit_verify(
    table: dict, *, policy_file: Path
) -> str | None:
    """Validate and return the optional ``submit.verify`` command (spec 00064).

    The standing build invariant re-run under the merge lock against the exact
    tree about to become the base. Returns ``None`` when absent so an
    unconfigured policy lands as before (no extra gate). A non-string or
    empty/whitespace-only value raises :class:`PolicyError` so a typo never
    silently degrades into "no gate."
    """
    value = table.get("verify")
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise PolicyError(
            f"{policy_file}: submit.verify must be a non-empty string"
        )
    return value


def _optional_submit_recovery_agent(
    table: dict, *, policy_file: Path
) -> tuple[int, float]:
    """Validate the merge strategy's bounded conflict-resolution session bounds.

    ``submit.recovery_agent_max_turns`` bounds the session's turns (``0``
    disables the rung, parking a merge conflict exactly as merge-fallback does);
    ``submit.recovery_agent_max_wall_seconds`` bounds its wall clock. Both fall
    back to their :data:`DEFAULT_RECOVERY_AGENT_MAX_TURNS` /
    :data:`DEFAULT_RECOVERY_AGENT_MAX_WALL_SECONDS` defaults when absent. A
    wrong-typed value (``bool`` counts as wrong here, as elsewhere) raises
    :class:`PolicyError` naming the offending key, and a negative value is
    rejected the same way so a typo never silently disables or unbounds the rung.
    """
    turns = table.get("recovery_agent_max_turns")
    if turns is None:
        max_turns = DEFAULT_RECOVERY_AGENT_MAX_TURNS
    elif isinstance(turns, bool) or not isinstance(turns, int) or turns < 0:
        raise PolicyError(
            f"{policy_file}: submit.recovery_agent_max_turns must be a "
            f"non-negative integer"
        )
    else:
        max_turns = turns

    wall = table.get("recovery_agent_max_wall_seconds")
    if wall is None:
        max_wall = DEFAULT_RECOVERY_AGENT_MAX_WALL_SECONDS
    elif (
        isinstance(wall, bool)
        or not isinstance(wall, (int, float))
        or wall <= 0
    ):
        raise PolicyError(
            f"{policy_file}: submit.recovery_agent_max_wall_seconds must be a "
            f"positive number"
        )
    else:
        max_wall = float(wall)

    return max_turns, max_wall


def _optional_sandbox_setup(
    table: dict, *, policy_file: Path
) -> str | None:
    """Validate and return the optional ``sandbox.setup`` command.

    Returns ``None`` when the key is absent so an unconfigured policy
    provisions sandboxes bare. A non-string or empty/whitespace-only value
    raises :class:`PolicyError` so a typo never silently degrades into
    "no setup."
    """
    value = table.get("setup")
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise PolicyError(
            f"{policy_file}: sandbox.setup must be a non-empty string"
        )
    return value


def _optional_phase_verify(
    table: dict, *, policy_file: Path
) -> str | None:
    """Validate and return the optional ``phase.verify`` command.

    Returns ``None`` when the key is absent so an unconfigured policy keeps
    today's archival (no phase-exit gate). A non-string or empty/whitespace-
    only value raises :class:`PolicyError` so a typo never silently degrades
    into "no gate."
    """
    value = table.get("verify")
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise PolicyError(
            f"{policy_file}: phase.verify must be a non-empty string"
        )
    return value


def _optional_held_out_root(
    table: dict, *, policy_file: Path
) -> Path | None:
    """Validate and return the optional ``held_out.root`` path.

    Returns ``None`` when the section (or the ``root`` key) is absent so an
    unconfigured policy never activates the execute-time held-out gate (spec
    00051, criterion #2, decision D-3): the worker builds no held-out source
    and landing stays byte-identical. A non-string or empty/whitespace-only
    value raises :class:`PolicyError` so a typo never silently leaves the gate
    inert. The path is returned as written; the worker resolves a relative
    value against the repo root so it points at ``<repo_root>/<root>``
    regardless of the worker's cwd (criterion #3).
    """
    value = table.get("root")
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise PolicyError(
            f"{policy_file}: held_out.root must be a non-empty string"
        )
    return Path(value)


def _optional_positive_int(
    table: dict, key: str, *, path: str, policy_file: Path
) -> int | None:
    """Validate an optional positive-integer override, ``None`` when absent.

    Returns ``None`` when the key is absent so the consumer keeps its own
    default (e.g. the harness's ``rubric_judge_max_turns`` of 32). A TOML
    boolean (``bool`` is an ``int`` subclass, so ``= true`` would otherwise
    read as ``1``), a non-integer, or a non-positive value raises
    :class:`PolicyError` naming ``path`` so a typo never silently degrades the
    budget.
    """
    if key not in table:
        return None
    value = table[key]
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise PolicyError(
            f"{policy_file}: {path} must be a positive integer"
        )
    return value


def _optional_deadlines(
    table: object, *, policy_file: Path
) -> DeadlineConfig:
    """Resolve the optional top-level ``[deadlines]`` table (spec 00066).

    Returns a default :class:`DeadlineConfig` (finite, non-null default-on
    ceilings) when the table is absent so every pre-existing policy file keeps
    loading byte-identically. When present it is resolved via
    :func:`flywheel_core.deadline_config.deadline_config_from_mapping` -- the
    spec-00066 key mapping consumed as-is, never forked -- so a key set to
    ``0`` opts that class out (unbounded). A non-table value, or a non-numeric
    per-class value (the resolver's :class:`ValueError`, wrapped), raises
    :class:`PolicyError` naming the file and the offending key.
    """
    if table is None:
        return DeadlineConfig()
    if not isinstance(table, dict):
        raise PolicyError(f"{policy_file}: [deadlines] must be a table")
    try:
        return deadline_config_from_mapping(table)
    except ValueError as exc:
        raise PolicyError(f"{policy_file}: {exc}") from exc


def _optional_autopilot(
    table: dict, *, policy_file: Path
) -> tuple[int, str, float, ScoreWeights | None]:
    """Validate the optional ``[autopilot]`` table.

    Returns ``(target_depth, landing, interval_seconds, weights)``. Absent keys
    take code defaults (a sane target depth, FF-merge landing, the default
    interval, and ``None`` weights -- the scoring engine's constant defaults).
    The optional ``[autopilot.weights]`` sub-table overrides individual score
    weights; an unset weight keeps the engine default. A malformed value raises
    :class:`PolicyError` so a typo never silently degrades autopilot's behavior.
    """
    target_depth = _override_int(
        table, "target_depth", DEFAULT_TARGET_DEPTH,
        path="autopilot.target_depth", policy_file=policy_file,
    )
    if target_depth <= 0:
        raise PolicyError(
            f"{policy_file}: autopilot.target_depth must be a positive integer"
        )
    landing = _override_str(
        table, "landing", DEFAULT_LANDING,
        path="autopilot.landing", policy_file=policy_file,
        choices=_AUTOPILOT_LANDINGS,
    )
    interval = _override_float(
        table, "interval_seconds", DEFAULT_AUTOPILOT_INTERVAL_SECONDS,
        path="autopilot.interval_seconds", policy_file=policy_file,
    )
    if interval <= 0:
        raise PolicyError(
            f"{policy_file}: autopilot.interval_seconds must be positive"
        )

    weights_tbl = table.get("weights")
    if weights_tbl is None:
        return target_depth, landing, interval, None
    if not isinstance(weights_tbl, dict):
        raise PolicyError(f"{policy_file}: [autopilot.weights] must be a table")
    base = ScoreWeights()
    weights = ScoreWeights(
        tier=_override_float(
            weights_tbl, "tier", base.tier,
            path="autopilot.weights.tier", policy_file=policy_file,
        ),
        urgency=_override_float(
            weights_tbl, "urgency", base.urgency,
            path="autopilot.weights.urgency", policy_file=policy_file,
        ),
        importance=_override_float(
            weights_tbl, "importance", base.importance,
            path="autopilot.weights.importance", policy_file=policy_file,
        ),
        unblock=_override_float(
            weights_tbl, "unblock", base.unblock,
            path="autopilot.weights.unblock", policy_file=policy_file,
        ),
        effort=_override_float(
            weights_tbl, "effort", base.effort,
            path="autopilot.weights.effort", policy_file=policy_file,
        ),
        interrupt_base=_override_float(
            weights_tbl, "interrupt_base", base.interrupt_base,
            path="autopilot.weights.interrupt_base", policy_file=policy_file,
        ),
    )
    return target_depth, landing, interval, weights


def _sandbox_subtable(
    table: dict, key: str, *, policy_file: Path, path: str
) -> dict:
    """Return a nested ``[sandbox.*]`` sub-table, or ``{}`` when absent.

    A present-but-non-table value fails fast so a scalar typed where a table
    belongs never silently degrades to defaults.
    """
    value = table.get(key)
    if value is None:
        return {}
    if not isinstance(value, dict):
        raise PolicyError(f"{policy_file}: [{path}] must be a table")
    return value


def _override_bool(
    table: dict, key: str, default: bool, *, path: str, policy_file: Path
) -> bool:
    if key not in table:
        return default
    value = table[key]
    if not isinstance(value, bool):
        raise PolicyError(f"{policy_file}: {path} must be a boolean")
    return value


def _override_int(
    table: dict, key: str, default: int, *, path: str, policy_file: Path
) -> int:
    if key not in table:
        return default
    value = table[key]
    # bool is an int subclass; reject it so ``= true`` is a typo, not 1.
    if isinstance(value, bool) or not isinstance(value, int):
        raise PolicyError(f"{policy_file}: {path} must be an integer")
    return value


def _override_float(
    table: dict, key: str, default: float, *, path: str, policy_file: Path
) -> float:
    if key not in table:
        return default
    value = table[key]
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise PolicyError(f"{policy_file}: {path} must be a number")
    return float(value)


def _override_str(
    table: dict,
    key: str,
    default: str,
    *,
    path: str,
    policy_file: Path,
    choices: tuple[str, ...] | None = None,
) -> str:
    if key not in table:
        return default
    value = table[key]
    if not isinstance(value, str) or not value.strip():
        raise PolicyError(f"{policy_file}: {path} must be a non-empty string")
    if choices is not None and value not in choices:
        raise PolicyError(
            f"{policy_file}: {path} must be one of {choices}, got {value!r}"
        )
    return value


def _override_str_tuple(
    table: dict,
    key: str,
    default: tuple[str, ...] | None,
    *,
    path: str,
    policy_file: Path,
) -> tuple[str, ...] | None:
    if key not in table:
        return default
    value = table[key]
    if not isinstance(value, list) or not all(
        isinstance(item, str) and item.strip() for item in value
    ):
        raise PolicyError(
            f"{policy_file}: {path} must be a list of non-empty strings"
        )
    return tuple(value)


def _override_str_dict(
    table: dict,
    key: str,
    default: dict[str, str],
    *,
    path: str,
    policy_file: Path,
) -> dict[str, str]:
    if key not in table:
        return default
    value = table[key]
    if not isinstance(value, dict) or not all(
        isinstance(k, str) and isinstance(v, str) for k, v in value.items()
    ):
        raise PolicyError(
            f"{policy_file}: {path} must be a table of string values"
        )
    return dict(value)


def _override_skills(
    table: dict,
    key: str,
    default: str | tuple[str, ...],
    *,
    path: str,
    policy_file: Path,
) -> str | tuple[str, ...]:
    """Resolve ``skills``: ``"all"``/``"none"`` (or any name) or a list."""
    if key not in table:
        return default
    value = table[key]
    if isinstance(value, str) and value.strip():
        return value
    if isinstance(value, list) and all(
        isinstance(item, str) and item.strip() for item in value
    ):
        return tuple(value)
    raise PolicyError(
        f"{policy_file}: {path} must be \"all\", \"none\", or a list of "
        f"non-empty strings"
    )


def _optional_sandbox_policy(
    sandbox: dict, *, policy_file: Path
) -> SandboxPolicy:
    """Resolve the ``[sandbox.*]`` tables into a frozen :class:`SandboxPolicy`.

    Selects the named preset (default ``fast``; an unknown name fails fast),
    then merges sparse per-key overrides from each sub-table onto that
    baseline with list-replace semantics. Absent keys (and unknown keys under
    any ``[sandbox.*]`` table) keep the preset value, mirroring the
    forward-compat ``_optional_*`` pattern. The existing flat ``setup`` key is
    left to :func:`_optional_sandbox_setup`; this resolver ignores it.
    """
    preset_name = sandbox.get("preset", "fast")
    base = (
        _SANDBOX_PRESETS.get(preset_name)
        if isinstance(preset_name, str)
        else None
    )
    if base is None:
        raise PolicyError(
            f"{policy_file}: sandbox.preset {preset_name!r} is not available "
            f"(defined presets: {tuple(_SANDBOX_PRESETS)})"
        )

    backend = _override_str(
        sandbox, "backend", base.backend,
        path="sandbox.backend", policy_file=policy_file,
    )
    permission_mode = _override_str(
        sandbox, "permission_mode", base.permission_mode,
        path="sandbox.permission_mode", policy_file=policy_file,
    )

    exec_tbl = _sandbox_subtable(
        sandbox, "exec", policy_file=policy_file, path="sandbox.exec"
    )
    sandbox_exec = SandboxExec(
        enabled=_override_bool(
            exec_tbl, "enabled", base.exec.enabled,
            path="sandbox.exec.enabled", policy_file=policy_file,
        ),
        auto_allow=_override_bool(
            exec_tbl, "auto_allow", base.exec.auto_allow,
            path="sandbox.exec.auto_allow", policy_file=policy_file,
        ),
    )

    cap_tbl = _sandbox_subtable(
        sandbox, "capabilities",
        policy_file=policy_file, path="sandbox.capabilities",
    )
    mcp_tbl = _sandbox_subtable(
        cap_tbl, "mcp",
        policy_file=policy_file, path="sandbox.capabilities.mcp",
    )
    capabilities = SandboxCapabilities(
        skills=_override_skills(
            cap_tbl, "skills", base.capabilities.skills,
            path="sandbox.capabilities.skills", policy_file=policy_file,
        ),
        allowed_tools=_override_str_tuple(
            cap_tbl, "allowed_tools", base.capabilities.allowed_tools,
            path="sandbox.capabilities.allowed_tools", policy_file=policy_file,
        )
        or (),
        denied_tools=_override_str_tuple(
            cap_tbl, "denied_tools", base.capabilities.denied_tools,
            path="sandbox.capabilities.denied_tools", policy_file=policy_file,
        )
        or (),
        setting_sources=_override_str_tuple(
            cap_tbl, "setting_sources", base.capabilities.setting_sources,
            path="sandbox.capabilities.setting_sources", policy_file=policy_file,
        ),
        mcp_servers=_override_str_tuple(
            mcp_tbl, "servers", base.capabilities.mcp_servers,
            path="sandbox.capabilities.mcp.servers", policy_file=policy_file,
        )
        or (),
        mcp_strict=_override_bool(
            mcp_tbl, "strict", base.capabilities.mcp_strict,
            path="sandbox.capabilities.mcp.strict", policy_file=policy_file,
        ),
    )

    net_tbl = _sandbox_subtable(
        sandbox, "network", policy_file=policy_file, path="sandbox.network"
    )
    network = SandboxNetwork(
        policy=_override_str(
            net_tbl, "policy", base.network.policy,
            path="sandbox.network.policy", policy_file=policy_file,
            choices=_NETWORK_POLICIES,
        ),
        allow_hosts=_override_str_tuple(
            net_tbl, "allow_hosts", base.network.allow_hosts,
            path="sandbox.network.allow_hosts", policy_file=policy_file,
        )
        or (),
        allow_unix_sockets=_override_str_tuple(
            net_tbl, "allow_unix_sockets", base.network.allow_unix_sockets,
            path="sandbox.network.allow_unix_sockets", policy_file=policy_file,
        )
        or (),
    )

    env_tbl = _sandbox_subtable(
        sandbox, "env", policy_file=policy_file, path="sandbox.env"
    )
    sandbox_env = SandboxEnv(
        passthrough=_override_str_tuple(
            env_tbl, "pass", base.env.passthrough,
            path="sandbox.env.pass", policy_file=policy_file,
        )
        or (),
        set_values=_override_str_dict(
            env_tbl, "set", base.env.set_values,
            path="sandbox.env.set", policy_file=policy_file,
        ),
        inherit_home=_override_bool(
            env_tbl, "inherit_home", base.env.inherit_home,
            path="sandbox.env.inherit_home", policy_file=policy_file,
        ),
    )

    limits_tbl = _sandbox_subtable(
        sandbox, "limits", policy_file=policy_file, path="sandbox.limits"
    )
    limits = SandboxLimits(
        max_turns=_override_int(
            limits_tbl, "max_turns", base.limits.max_turns,
            path="sandbox.limits.max_turns", policy_file=policy_file,
        ),
        max_retries=_override_int(
            limits_tbl, "max_retries", base.limits.max_retries,
            path="sandbox.limits.max_retries", policy_file=policy_file,
        ),
        lease_seconds=_override_int(
            limits_tbl, "lease_seconds", base.limits.lease_seconds,
            path="sandbox.limits.lease_seconds", policy_file=policy_file,
        ),
        wall_clock_seconds=_override_int(
            limits_tbl, "wall_clock_seconds", base.limits.wall_clock_seconds,
            path="sandbox.limits.wall_clock_seconds", policy_file=policy_file,
        ),
        max_cost_usd=_override_float(
            limits_tbl, "max_cost_usd", base.limits.max_cost_usd,
            path="sandbox.limits.max_cost_usd", policy_file=policy_file,
        ),
        max_tokens=_override_int(
            limits_tbl, "max_tokens", base.limits.max_tokens,
            path="sandbox.limits.max_tokens", policy_file=policy_file,
        ),
        rubric_judge_max_turns=_optional_positive_int(
            limits_tbl, "rubric_judge_max_turns",
            path="sandbox.limits.rubric_judge_max_turns",
            policy_file=policy_file,
        ),
    )

    retention_tbl = _sandbox_subtable(
        sandbox, "retention",
        policy_file=policy_file, path="sandbox.retention",
    )
    retention = SandboxRetention(
        on_done=_override_str(
            retention_tbl, "on_done", base.retention.on_done,
            path="sandbox.retention.on_done", policy_file=policy_file,
            choices=_RETENTION_ON_DONE,
        ),
        on_failure=_override_str(
            retention_tbl, "on_failure", base.retention.on_failure,
            path="sandbox.retention.on_failure", policy_file=policy_file,
            choices=_RETENTION_ON_FAILURE,
        ),
        sweep_days=_override_int(
            retention_tbl, "sweep_days", base.retention.sweep_days,
            path="sandbox.retention.sweep_days", policy_file=policy_file,
        ),
    )

    container_tbl = _sandbox_subtable(
        sandbox, "container", policy_file=policy_file, path="sandbox.container"
    )
    container = SandboxContainer(
        image=_override_str(
            container_tbl, "image", base.container.image,
            path="sandbox.container.image", policy_file=policy_file,
        ),
        model=_override_str(
            container_tbl, "model", base.container.model,
            path="sandbox.container.model", policy_file=policy_file,
        ),
        auth=_override_str(
            container_tbl, "auth", base.container.auth,
            path="sandbox.container.auth", policy_file=policy_file,
            choices=_CONTAINER_AUTH_MODES,
        ),
        auth_env=_override_str(
            container_tbl, "auth_env", base.container.auth_env,
            path="sandbox.container.auth_env", policy_file=policy_file,
        ),
        exec_timeout=_override_int(
            container_tbl, "exec_timeout", base.container.exec_timeout,
            path="sandbox.container.exec_timeout", policy_file=policy_file,
        ),
        egress_network=_override_str(
            container_tbl, "egress_network", base.container.egress_network,
            path="sandbox.container.egress_network", policy_file=policy_file,
        ),
    )
    if backend == "container" and not container.image:
        raise PolicyError(
            f"{policy_file}: [sandbox.container] image is required when "
            f"sandbox.backend = 'container'"
        )

    return SandboxPolicy(
        preset=preset_name,
        backend=backend,
        permission_mode=permission_mode,
        exec=sandbox_exec,
        capabilities=capabilities,
        network=network,
        env=sandbox_env,
        limits=limits,
        retention=retention,
        container=container,
    )


def _make_stop_sink(
    control: StopEventStore | None,
    now: Callable[[], datetime] | None,
) -> StopEventSink | None:
    """Bind a source's stop-event sink to the durable stop-event ledger.

    Returns ``None`` when no ``control`` store is supplied, so a source built
    without one behaves byte-for-byte as before (the sink is an audit witness,
    never a scheduling input). When bound, each ``sink(kind, subject, detail)``
    call appends one append-only ``record_stop_event`` row stamped with the
    injected ``now`` clock (falling back to wall-clock UTC).
    """
    if control is None:
        return None
    clock = now if now is not None else (lambda: datetime.now(timezone.utc))

    def sink(kind: str, subject: str, detail: str) -> None:
        control.record_stop_event(
            kind=kind, subject=subject, detail=detail, occurred_at=clock()
        )

    return sink


def build_directory_source(
    policy: WorkPolicy,
    *,
    control: StopEventStore | None = None,
    now: Callable[[], datetime] | None = None,
) -> WorkSource:
    """Build the directory backend (the registry's ``directory`` target).

    ``control`` / ``now`` are accepted for a uniform builder signature but
    unused here: the directory source has no truncation or zero-grader gate --
    an unloadable task file surfaces as a raised ``TaskLoadError`` (a loud
    abort), so it has no pre-run dead-end to witness on the stop ledger.
    """
    assert policy.tasks_dir is not None  # load_policy guarantees it
    return DirectoryWorkSource(policy.tasks_dir)


def build_github_source(
    policy: WorkPolicy,
    *,
    control: StopEventStore | None = None,
    now: Callable[[], datetime] | None = None,
) -> WorkSource:
    """Build the GitHub-issues backend (the registry's ``github`` target)."""
    assert policy.github_repo is not None and policy.github_label is not None
    return GithubWorkSource(
        repo=policy.github_repo,
        label=policy.github_label,
        default_graders=policy.default_graders,
        done_action=policy.github_done_action,
        stop_sink=_make_stop_sink(control, now),
    )


def build_github_ci_source(
    policy: WorkPolicy,
    *,
    control: StopEventStore | None = None,
    now: Callable[[], datetime] | None = None,
) -> WorkSource:
    """Build the GitHub-CI backend (the registry's ``github_ci`` target)."""
    assert policy.github_ci_repo is not None  # load_policy guarantees it
    return GithubCiWorkSource(
        repo=policy.github_ci_repo,
        default_graders=policy.default_graders,
        failure_filter=policy.github_ci_failure_filter,
        stop_sink=_make_stop_sink(control, now),
    )


def build_github_review_source(
    policy: WorkPolicy,
    *,
    control: StopEventStore | None = None,
    now: Callable[[], datetime] | None = None,
) -> WorkSource:
    """Build the PR-review backend (the registry's ``github_review`` target).

    Binds a :class:`GithubReviewWorkSource` to the policy's repo and the
    operator's default graders -- the grade is the harness's out-of-band
    graders, never the thread's resolution state (spec 00053, D-4).
    """
    assert policy.github_review_repo is not None  # load_policy guarantees it
    return GithubReviewWorkSource(
        repo=policy.github_review_repo,
        default_graders=policy.default_graders,
        stop_sink=_make_stop_sink(control, now),
    )


def build_work_source(
    policy: WorkPolicy,
    *,
    control: StopEventStore | None = None,
    now: Callable[[], datetime] | None = None,
) -> WorkSource:
    """Construct the :class:`WorkSource` a policy describes.

    Routes ``policy.source_kind`` through the
    :data:`~flywheel_orchestrator._source_registry.SOURCES` registry; the
    per-kind construction lives in :func:`build_directory_source` /
    :func:`build_github_source`. When ``control`` is supplied it is forwarded
    to the resolved builder, which wires each source's ``source-truncation`` /
    ``zero-grader-drop`` stop events to that durable ledger.
    """
    return SOURCES.resolve(policy.source_kind)(policy, control=control, now=now)


__all__ = [
    "DEFAULT_CHECKPOINT_NUDGE_SECONDS",
    "DEFAULT_POLICY_FILENAME",
    "DEFAULT_SESSION_PAUSE_CEILING_SECONDS",
    "DEFAULT_WORKER_CONCURRENCY",
    "PolicyError",
    "SandboxCapabilities",
    "SandboxEnv",
    "SandboxExec",
    "SandboxLimits",
    "SandboxNetwork",
    "SandboxPolicy",
    "SandboxRetention",
    "WorkPolicy",
    "build_directory_source",
    "build_github_ci_source",
    "build_github_review_source",
    "build_github_source",
    "build_work_source",
    "load_policy",
    "resolve_grader_env",
]

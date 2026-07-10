"""Held-out oracle for spec 00044 G5 — ContainerSubmitStrategy.

The strategy composes an inner landing strategy and a ContainerRuntime seam;
prepare_sandbox starts a container bind-mounting the inner worktree and
returns a SandboxHandle whose invoke_wrapper runs the agent via the
flywheel-agents claude-code CLI transport (docs/agent-harness.md 15.4) and
whose teardown disposes it; submit delegates to the inner strategy.

Container lifecycle is exercised with fakes — no Docker. The invoke path is
exercised END TO END through the real bridge (make_agents_invoke), runtime,
and adapter: the ``invoke_factory`` seam swaps only the docker hop for a
LocalHost running a scripted stream-json executable (the pattern of
``flywheel-core/tests/test_agents_invoke.py``). No mocks of flywheel logic.
Do not weaken assertions.
"""

from __future__ import annotations

import asyncio
import sys
import textwrap
from collections.abc import Awaitable
from pathlib import Path

from flywheel_agents import LocalHost
from flywheel_core import InvocationRequest, Intent, IterationResult, ValidEnvelope
from flywheel_core.agents_invoke import make_agents_invoke
from flywheel_core.harness import InvokeFunc
from flywheel_orchestrator import SandboxHandle, SandboxRequest, SubmitRequest

from flywheel_container import ContainerRuntime, ContainerSubmitStrategy


async def _await_invoke(
    awaitable: Awaitable[IterationResult],
) -> IterationResult:
    """Drive an ``InvokeFunc`` result (an ``Awaitable``) under ``asyncio.run``,
    which requires a coroutine."""
    return await awaitable


def _request(task_id: str = "alpha") -> SandboxRequest:
    return SandboxRequest(
        task_id=task_id, task_file=Path(), run_id=None, mode="fresh"
    )


class _FakeInner:
    def __init__(self, sandbox: Path, *, handle: SandboxHandle | None = None) -> None:
        self._sandbox = sandbox
        self._handle = handle
        self.submitted: list[SubmitRequest] = []

    def prepare_sandbox(self, request: SandboxRequest):
        return self._handle if self._handle is not None else self._sandbox

    def submit(self, request: SubmitRequest) -> None:
        self.submitted.append(request)


class _RuntimeRecorder:
    def __init__(self) -> None:
        self.start_calls: list[dict] = []
        self.removed: list[str] = []
        self.unregistered: list[str] = []

    def start(self, name, image, **kwargs):
        self.start_calls.append({"name": name, "image": image, **kwargs})
        return name

    def remove(self, name) -> None:
        self.removed.append(name)

    def register_cleanup(self, name):
        def _unregister() -> None:
            self.unregistered.append(name)

        return _unregister

    def as_runtime(self) -> ContainerRuntime:
        return ContainerRuntime(
            start=self.start,
            remove=self.remove,
            register_cleanup=self.register_cleanup,
        )


def _strategy(inner, rec, **kwargs):
    return ContainerSubmitStrategy(
        inner,
        image="flywheel/agent:test",
        model="claude-opus-4-8",
        container_uid=1000,
        container_gid=1000,
        preflight=False,
        runtime=rec.as_runtime(),
        **kwargs,
    )


def test_prepare_bind_mounts_worktree_and_returns_handle(tmp_path: Path) -> None:
    worktree = tmp_path / "wt"
    worktree.mkdir()
    inner = _FakeInner(worktree)
    rec = _RuntimeRecorder()
    handle = _strategy(inner, rec).prepare_sandbox(_request())

    assert isinstance(handle, SandboxHandle)
    assert handle.path == worktree
    assert handle.invoke_wrapper is not None
    assert handle.teardown is not None
    # The worktree is bind-mounted at the workdir, container runs as the uid.
    start = rec.start_calls[0]
    assert start["user"] == "1000:1000"
    mount = start["mounts"][0]
    assert mount.host_path == str(worktree)
    assert mount.sandbox_path.endswith("/workspace")


def test_container_always_gets_home_env(tmp_path: Path) -> None:
    # A numeric --user leaves HOME unset; the agent CLI needs it for ~/.claude.
    rec = _RuntimeRecorder()
    _strategy(_FakeInner(tmp_path), rec).prepare_sandbox(_request())
    from flywheel_container import DEFAULT_AGENT_HOME

    assert rec.start_calls[0]["env"]["HOME"] == DEFAULT_AGENT_HOME


def test_operator_home_override_is_respected(tmp_path: Path) -> None:
    rec = _RuntimeRecorder()
    ContainerSubmitStrategy(
        _FakeInner(tmp_path),
        image="img",
        model="m",
        env={"HOME": "/custom/home"},
        container_uid=1000,
        container_gid=1000,
        preflight=False,
        runtime=rec.as_runtime(),
    ).prepare_sandbox(_request())
    assert rec.start_calls[0]["env"]["HOME"] == "/custom/home"


def test_teardown_removes_container_and_unregisters(tmp_path: Path) -> None:
    inner = _FakeInner(tmp_path)
    rec = _RuntimeRecorder()
    handle = _strategy(inner, rec).prepare_sandbox(_request())
    assert handle.teardown is not None
    handle.teardown()
    assert len(rec.removed) == 1
    assert len(rec.unregistered) == 1


def test_default_invoke_wrapper_builds_invoke_and_ignores_base(
    tmp_path: Path,
) -> None:
    # The default factory (make_agents_invoke over DockerExecHost) constructs
    # without touching docker; the base invoke is intentionally discarded.
    rec = _RuntimeRecorder()
    handle = _strategy(_FakeInner(tmp_path), rec).prepare_sandbox(_request())
    assert handle.invoke_wrapper is not None

    async def _base(_req: InvocationRequest):  # the wrapper must ignore this
        raise AssertionError("base invoke must not run in the container path")

    invoke = handle.invoke_wrapper(_base)
    assert callable(invoke)
    assert invoke is not _base


# --- the rebased invoke path, end to end without docker ----------------------
# A scripted stream-json executable stands in for the claude CLI; the injected
# invoke_factory builds the SAME InvokeFunc the default factory does, with only
# the DockerExecHost hop swapped for LocalHost + command_override.

_SCRIPT_OK = textwrap.dedent(
    """
    import json
    import sys

    prompt = sys.stdin.read()
    def emit(obj):
        print(json.dumps(obj), flush=True)

    emit({"type": "system", "subtype": "init", "session_id": "sess-ct"})
    envelope = (
        "prompt=" + prompt + "\\n<!-- LOOP_STATUS -->\\n"
        + json.dumps({"intent": "verify", "reason": "complete"})
        + "\\n<!-- /LOOP_STATUS -->"
    )
    emit(
        {
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": envelope}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 11, "output_tokens": 2},
            },
        }
    )
    emit(
        {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "num_turns": 1,
            "total_cost_usd": 0.02,
            "usage": {"input_tokens": 11, "output_tokens": 2},
        }
    )
    """
)

_SCRIPT_EXIT_3 = textwrap.dedent(
    """
    import json
    import sys

    sys.stdin.read()
    print(json.dumps({"type": "system", "subtype": "init"}), flush=True)
    sys.exit(3)
    """
)

_SCRIPT_HANG = textwrap.dedent(
    """
    import json
    import sys
    import time

    print(json.dumps({"type": "system", "subtype": "init"}), flush=True)
    time.sleep(30)
    """
)


def _local_factory(script: Path, workdir: Path, *, timeout: float | None = 30.0):
    """An invoke_factory mirroring the strategy's default builder, with the
    docker hop swapped for a local scripted agent."""

    def factory(container_name: str) -> InvokeFunc:
        assert container_name  # the per-run name reaches the factory
        return make_agents_invoke(
            agent_id="claude-code",
            working_directory=workdir,
            model="claude-opus-4-8",
            permission_policy="auto",
            timeout_seconds=timeout,
            host=LocalHost(),
            command_override=(sys.executable, str(script)),
        )

    return factory


def _invoke_via_strategy(
    tmp_path: Path, script_body: str, *, timeout: float | None = 30.0
) -> tuple[IterationResult, list[object]]:
    script = tmp_path / "scripted_agent.py"
    script.write_text(script_body)
    rec = _RuntimeRecorder()
    strategy = _strategy(
        _FakeInner(tmp_path),
        rec,
        invoke_factory=_local_factory(script, tmp_path, timeout=timeout),
    )
    handle = strategy.prepare_sandbox(_request())
    assert handle.invoke_wrapper is not None

    async def _base(_req: InvocationRequest):
        raise AssertionError("base invoke must not run in the container path")

    invoke = handle.invoke_wrapper(_base)
    observed: list[object] = []
    request = InvocationRequest(
        prompt="do the thing",
        transcript_graders=(),
        attempt_number=1,
        iteration_number=1,
        on_message=observed.append,
    )
    return asyncio.run(_await_invoke(invoke(request))), observed


def test_invoke_wrapper_runs_agent_end_to_end(tmp_path: Path) -> None:
    result, observed = _invoke_via_strategy(tmp_path, _SCRIPT_OK)

    # The prompt reached the agent on stdin and rode back in the transcript.
    assert "prompt=do the thing" in result.transcript
    # Parsed by the flywheel-agents adapter: envelope, signals, usage, cost.
    assert isinstance(result.envelope, ValidEnvelope)
    assert result.envelope.intent is Intent.VERIFY
    assert result.signals.session_id == "sess-ct"
    assert result.signals.stop_reason == "end_turn"
    assert result.signals.result_subtype == "success"
    assert result.signals.total_cost_usd == 0.02
    assert result.usage == {
        "input_tokens": 11,
        "output_tokens": 2,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }
    assert result.failure is None
    # Normalized events reached the harness observer — the hang watchdog and
    # per-message telemetry are live on the container path now.
    assert observed, "on_message never called"


def test_invoke_nonzero_exit_sets_agent_exit_failure(tmp_path: Path) -> None:
    result, _ = _invoke_via_strategy(tmp_path, _SCRIPT_EXIT_3)
    assert result.failure is not None
    assert result.failure.error_type == "agent_exit"
    assert result.failure.exit_code == 3


def test_invoke_timeout_is_structured_failure(tmp_path: Path) -> None:
    # exec_timeout expiry no longer raises; it folds into a structured
    # failure the harness can classify.
    result, _ = _invoke_via_strategy(tmp_path, _SCRIPT_HANG, timeout=0.5)
    assert result.failure is not None
    assert result.failure.error_type == "timeout"


def test_submit_delegates_to_inner(tmp_path: Path) -> None:
    inner = _FakeInner(tmp_path)
    rec = _RuntimeRecorder()
    strategy = _strategy(inner, rec)
    req = SubmitRequest(
        task_id="alpha",
        task_file=Path(),
        task=None,  # type: ignore[arg-type]
        run_id="r1",
        status=None,  # type: ignore[arg-type]
        sandbox=tmp_path,
    )
    strategy.submit(req)
    assert inner.submitted == [req]


def test_inner_sandbox_handle_env_contribution_is_preserved(tmp_path: Path) -> None:
    inner_handle = SandboxHandle(path=tmp_path, env_contribution={"FOO": "bar"})
    inner = _FakeInner(tmp_path, handle=inner_handle)
    rec = _RuntimeRecorder()
    handle = _strategy(inner, rec).prepare_sandbox(_request())
    assert handle.env_contribution == {"FOO": "bar"}

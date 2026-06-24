"""Held-out oracle for spec 00044 G5 — ContainerSubmitStrategy.

RED until G5 lands. The strategy composes an inner landing strategy and a
ContainerRuntime seam; prepare_sandbox starts a container bind-mounting the
inner worktree and returns a SandboxHandle whose invoke_wrapper runs the agent
CLI in the container (parsed via G4) and whose teardown disposes it; submit
delegates to the inner strategy. Exercised with fakes — no Docker. Do not
weaken assertions.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

from flywheel_core import InvocationRequest, Intent, ValidEnvelope
from flywheel_orchestrator import SandboxHandle, SandboxRequest, SubmitRequest

from flywheel_container import (
    ClaudeCliAgent,
    ContainerRuntime,
    ContainerSubmitStrategy,
    ExecResult,
)
from flywheel_core.envelope import OPENING_FENCE, CLOSING_FENCE


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
    def __init__(self, *, lines: list[str] | None = None, exit_code: int = 0) -> None:
        self.start_calls: list[dict] = []
        self.exec_calls: list[dict] = []
        self.removed: list[str] = []
        self.unregistered: list[str] = []
        self._lines = lines or []
        self._exit_code = exit_code

    def start(self, name, image, **kwargs):
        self.start_calls.append({"name": name, "image": image, **kwargs})
        return name

    def exec_command(self, name, command, *, stdin=None, on_line=None, cwd=None, **kw):
        self.exec_calls.append(
            {"name": name, "command": command, "stdin": stdin, "cwd": cwd}
        )
        if on_line is not None:
            for line in self._lines:
                on_line(line)
        return ExecResult(stdout="", stderr="boom" if self._exit_code else "", exit_code=self._exit_code)

    def remove(self, name) -> None:
        self.removed.append(name)

    def register_cleanup(self, name):
        def _unregister() -> None:
            self.unregistered.append(name)

        return _unregister

    def as_runtime(self) -> ContainerRuntime:
        return ContainerRuntime(
            start=self.start,
            exec_command=self.exec_command,
            remove=self.remove,
            register_cleanup=self.register_cleanup,
        )


def _strategy(inner, rec, *, exit_code=0):
    return ContainerSubmitStrategy(
        inner,
        image="flywheel/agent:test",
        agent=ClaudeCliAgent(model="claude-opus-4-8"),
        container_uid=1000,
        container_gid=1000,
        preflight=False,
        runtime=rec.as_runtime(),
    )


def test_claude_cli_agent_build_command() -> None:
    cmd, stdin = ClaudeCliAgent(model="claude-opus-4-8").build_command("the prompt")
    assert cmd == (
        "claude --print --verbose --output-format stream-json "
        "--model claude-opus-4-8 -p -"
    )
    assert stdin == "the prompt"


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


def test_teardown_removes_container_and_unregisters(tmp_path: Path) -> None:
    inner = _FakeInner(tmp_path)
    rec = _RuntimeRecorder()
    handle = _strategy(inner, rec).prepare_sandbox(_request())
    assert handle.teardown is not None
    handle.teardown()
    assert len(rec.removed) == 1
    assert len(rec.unregistered) == 1


def test_invoke_wrapper_runs_agent_and_parses_stream(tmp_path: Path) -> None:
    envelope = f'{OPENING_FENCE}\n{{"intent": "verify"}}\n{CLOSING_FENCE}'
    lines = [
        json.dumps({"type": "system", "subtype": "init", "session_id": "s1"}),
        json.dumps(
            {
                "type": "assistant",
                "message": {
                    "content": [{"type": "text", "text": "ready\n" + envelope}],
                    "usage": {"input_tokens": 11},
                },
            }
        ),
        json.dumps(
            {
                "type": "result",
                "subtype": "success",
                "result": "ok",
                "total_cost_usd": 0.02,
                "num_turns": 1,
                "is_error": False,
            }
        ),
    ]
    inner = _FakeInner(tmp_path)
    rec = _RuntimeRecorder(lines=lines)
    handle = _strategy(inner, rec).prepare_sandbox(_request())
    assert handle.invoke_wrapper is not None

    async def _base(_req: InvocationRequest):  # the wrapper must ignore this
        raise AssertionError("base invoke must not run in the container path")

    invoke = handle.invoke_wrapper(_base)
    request = InvocationRequest(
        prompt="do the thing",
        transcript_graders=(),
        attempt_number=1,
        iteration_number=1,
    )
    result = asyncio.run(invoke(request))

    # Agent ran in the container with the prompt piped on stdin.
    call = rec.exec_calls[0]
    assert call["stdin"] == "do the thing"
    assert call["cwd"].endswith("/workspace")
    assert "claude --print" in call["command"]
    # Parsed via G4: envelope, usage, cost.
    assert isinstance(result.envelope, ValidEnvelope)
    assert result.envelope.intent is Intent.VERIFY
    assert result.usage == {
        "input_tokens": 11,
        "output_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }
    assert result.signals.total_cost_usd == 0.02
    assert result.failure is None


def test_invoke_nonzero_exit_sets_failure(tmp_path: Path) -> None:
    inner = _FakeInner(tmp_path)
    rec = _RuntimeRecorder(lines=[], exit_code=1)
    handle = _strategy(inner, rec).prepare_sandbox(_request())
    assert handle.invoke_wrapper is not None

    async def _base(_req: InvocationRequest):
        raise AssertionError("unused")

    invoke = handle.invoke_wrapper(_base)
    result = asyncio.run(
        invoke(
            InvocationRequest(
                prompt="p",
                transcript_graders=(),
                attempt_number=1,
                iteration_number=1,
            )
        )
    )
    assert result.failure is not None
    assert result.failure.exit_code == 1


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

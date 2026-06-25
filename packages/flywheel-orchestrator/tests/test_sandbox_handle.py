"""Held-out oracle for spec 00043 (SandboxHandle isolation seam, increment F of
00036).

RED until ``sandbox-handle-seam`` lands. ``SandboxProvider`` may return a
``SandboxHandle`` (path + env_contribution + invoke_wrapper); a bare ``Path`` is
adapted to an empty-contribution handle (``_as_handle``) and the orchestrator
folds the handle's contributions into the per-run drive args (``_apply_handle``)
— an empty handle being byte-identical to today. Do not weaken assertions.
"""

from __future__ import annotations

from pathlib import Path

from flywheel_core import InvocationRequest, IterationResult
from flywheel_orchestrator import SandboxHandle
from flywheel_orchestrator._orchestrate import (
    _apply_handle,
    _sandbox_agent_primitives,
)
from flywheel_orchestrator._strategy import _as_handle


async def _invoke(request: InvocationRequest) -> IterationResult:  # a minimal InvokeFunc
    raise AssertionError("base invoke must not be called by the seam")


def test_as_handle_wraps_bare_path(tmp_path: Path) -> None:
    h = _as_handle(tmp_path)
    assert isinstance(h, SandboxHandle)
    assert h.path == tmp_path
    assert h.env_contribution == {}
    assert h.invoke_wrapper is None


def test_as_handle_passes_handle_through(tmp_path: Path) -> None:
    orig = SandboxHandle(path=tmp_path, env_contribution={"A": "1"})
    assert _as_handle(orig) is orig


def test_apply_handle_empty_is_identity(tmp_path: Path) -> None:
    prims = _sandbox_agent_primitives(None)
    eff, inv = _apply_handle(SandboxHandle(path=tmp_path), prims, _invoke)
    assert eff is prims  # untouched object — proves back-compat
    assert inv is _invoke


def test_apply_handle_merges_env_contribution_handle_wins(tmp_path: Path) -> None:
    prims = dict(_sandbox_agent_primitives(None))
    prims["agent_env"] = {"BASE": "b", "SHARED": "policy"}
    h = SandboxHandle(
        path=tmp_path,
        env_contribution={"SHARED": "handle", "EXTRA": "x"},
    )
    eff, inv = _apply_handle(h, prims, _invoke)
    assert eff["agent_env"] == {"BASE": "b", "SHARED": "handle", "EXTRA": "x"}
    # the original policy env is not mutated
    assert prims["agent_env"] == {"BASE": "b", "SHARED": "policy"}
    assert inv is _invoke


def test_apply_handle_wraps_invoke(tmp_path: Path) -> None:
    seen: dict[str, object] = {}

    async def wrapped(request: object) -> object:
        return "ran-in-sandbox"

    def wrapper(inner: object) -> object:
        seen["inner"] = inner
        return wrapped

    h = SandboxHandle(path=tmp_path, invoke_wrapper=wrapper)  # type: ignore[arg-type]
    eff, inv = _apply_handle(h, _sandbox_agent_primitives(None), _invoke)
    assert inv is wrapped
    assert seen["inner"] is _invoke


def test_apply_handle_invoke_wrapper_runs_with_none_base(tmp_path: Path) -> None:
    # In normal operation orchestrate is driven with invoke=None (the SDK
    # invoker is built downstream). A replacing wrapper (container backend)
    # must still be applied — it is handed the None base and ignores it.
    seen: dict[str, object] = {}

    async def replacement(request: object) -> object:
        return "in-container"

    def wrapper(inner: object) -> object:
        seen["inner"] = inner
        return replacement

    h = SandboxHandle(path=tmp_path, invoke_wrapper=wrapper)  # type: ignore[arg-type]
    eff, inv = _apply_handle(h, _sandbox_agent_primitives(None), None)
    assert inv is replacement
    assert seen["inner"] is None  # base invoke was None, passed through

"""Held-out oracle for spec 00066 criterion 5 — the *policy-resolved* ceiling.

The sibling ``test_docker_management_deadline.py`` proves the primitives are
default-bounded. This suite proves the operator override *reaches* the
management calls the container strategy issues: a ``ContainerSubmitStrategy``
built with a custom ``management_timeout`` must apply it to every docker
*management* seam -- the preflight ``check_image_uid``, the internal-network
ensure, container teardown, and (criterion 5's named case) the ``atexit``
force-remove backstop -- while ``start_container`` keeps its own 120s bound.

The proofs are seam-level: a stub ``subprocess.run`` records the ``timeout`` it
receives (so the unbounded ``None`` opt-out is asserted at the call boundary,
never by waiting), plus one genuinely hanging child to show the configured
ceiling really cuts a wedged ``dockerd`` off in bounded wall time.

The ``atexit`` assertions are the ones that fail if only the inline call sites
are fixed and ``register_container_cleanup`` is left unbound.

Do not weaken assertions.
"""

from __future__ import annotations

import subprocess
import time
from pathlib import Path

import pytest

from flywheel_core.deadline_config import DEFAULT_DOCKER_MANAGEMENT_SECONDS

from flywheel_container import (
    DEFAULT_MANAGEMENT_TIMEOUT,
    ClaudeCliAgent,
    ContainerSubmitStrategy,
    DockerTimeoutError,
    build_container_strategy,
)
from flywheel_container import _docker
from flywheel_orchestrator import SandboxRequest, SubmitRequest

# A distinctive finite ceiling that is NOT the module default (120s), so a
# recorded ``120.0`` at a management seam proves the override never arrived.
_CUSTOM_CEILING = 3.0
# A tiny ceiling for the real-hang path: a hung child must be cut off well
# within the wall budget. Both are real elapsed wall-clock, not a fake clock.
_TINY_CEILING = 0.2
_WALL_BUDGET = 5.0

assert _CUSTOM_CEILING != DEFAULT_DOCKER_MANAGEMENT_SECONDS


class _FakeInner:
    """Inner landing strategy stand-in: hands back a worktree path, no docker."""

    def __init__(self, sandbox: Path) -> None:
        self._sandbox = sandbox

    def prepare_sandbox(self, request: SandboxRequest) -> Path:
        return self._sandbox

    def submit(self, request: SubmitRequest) -> None:  # pragma: no cover
        pass


def _request(task_id: str = "alpha") -> SandboxRequest:
    return SandboxRequest(
        task_id=task_id, task_file=Path(), run_id=None, mode="fresh"
    )


def _strategy(
    tmp_path: Path,
    *,
    management_timeout: float | None,
    preflight: bool = False,
    network_policy: str | None = None,
) -> ContainerSubmitStrategy:
    """A real strategy over the default runtime (no injected fakes), so the
    docker-management ceiling is threaded through the real primitives."""
    return ContainerSubmitStrategy(
        _FakeInner(tmp_path),
        image="img",
        agent=ClaudeCliAgent(model="m"),
        container_uid=1000,
        container_gid=1000,
        network_policy=network_policy,
        preflight=preflight,
        management_timeout=management_timeout,
    )


def _install_capturing_run(
    monkeypatch: pytest.MonkeyPatch,
) -> list[tuple[str, float | None]]:
    """Record ``(docker subcommand, timeout)`` per ``subprocess.run`` and
    succeed, so no wall-clock wait is involved -- the opt-out is asserted at the
    call seam, not by elapsed time."""
    records: list[tuple[str, float | None]] = []

    def capturing_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        sub = argv[1] if len(argv) > 1 else ""
        records.append((sub, kwargs.get("timeout")))
        return subprocess.CompletedProcess(argv, 0, "id\n", "")

    monkeypatch.setattr("flywheel_container._docker.subprocess.run", capturing_run)
    return records


def _install_hanging_docker(monkeypatch: pytest.MonkeyPatch) -> None:
    """Redirect ``subprocess.run`` at a real hanging child that honors the
    passed ``timeout`` (a wedged ``dockerd`` modeled by a long ``sleep``)."""
    real_run = subprocess.run

    def hanging_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        return real_run(
            ["sleep", "30"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=kwargs.get("timeout"),
        )

    monkeypatch.setattr("flywheel_container._docker.subprocess.run", hanging_run)


# --- the configured ceiling reaches every inline management seam --------------


def test_configured_ceiling_reaches_all_management_seams(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Isolate the crash-cleanup registry so this test's registrations do not
    # touch (or get touched by) the process-global one.
    monkeypatch.setattr(_docker, "_registered", {})
    records = _install_capturing_run(monkeypatch)

    strategy = _strategy(
        tmp_path,
        management_timeout=_CUSTOM_CEILING,
        preflight=True,
        network_policy="deny",
    )
    handle = strategy.prepare_sandbox(_request())
    assert handle.teardown is not None
    handle.teardown()

    # check_image_uid, ensure_internal_network (via network_exists), and
    # remove_container (stop + rm) all ran under the operator ceiling...
    assert ("image", _CUSTOM_CEILING) in records
    assert ("network", _CUSTOM_CEILING) in records
    assert ("stop", _CUSTOM_CEILING) in records
    assert ("rm", _CUSTOM_CEILING) in records
    # ...while start_container keeps its own, already-bounded 120s default.
    assert ("run", 120.0) in records


# --- criterion 5: the atexit force-remove honors the configured ceiling -------


def test_atexit_force_remove_honors_configured_ceiling(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(_docker, "_registered", {})
    records = _install_capturing_run(monkeypatch)

    strategy = _strategy(tmp_path, management_timeout=_CUSTOM_CEILING)
    strategy.prepare_sandbox(_request())  # registers the container for cleanup

    records.clear()  # drop the start call; keep only the flush's force-remove
    _docker._flush_cleanup_registry()

    # docker rm -f under the configured ceiling -- fixing only the inline call
    # sites (leaving register_container_cleanup unbound) records 120.0 here.
    assert records == [("rm", _CUSTOM_CEILING)]


# --- a genuinely hung dockerd is cut off in bounded wall time -----------------


def test_configured_ceiling_bounds_a_hung_daemon(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(_docker, "_registered", {})
    _install_hanging_docker(monkeypatch)

    strategy = _strategy(
        tmp_path, management_timeout=_TINY_CEILING, preflight=True
    )
    started = time.monotonic()
    with pytest.raises(DockerTimeoutError):
        # The first management call (preflight check_image_uid) hangs; the tiny
        # ceiling reached it, so control returns as a distinguishable timeout.
        strategy.prepare_sandbox(_request())
    assert time.monotonic() - started < _WALL_BUDGET


# --- the unbounded opt-out flows through, asserted at the seam (no waiting) ----


def test_unbounded_opt_out_flows_to_seams_including_atexit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(_docker, "_registered", {})
    records = _install_capturing_run(monkeypatch)

    strategy = _strategy(
        tmp_path,
        management_timeout=None,
        preflight=True,
        network_policy="deny",
    )
    strategy.prepare_sandbox(_request())  # leave registered for the flush below

    # The inline seams received timeout=None at the call boundary...
    assert ("image", None) in records
    assert ("network", None) in records
    # ...start still carries its own finite bound, untouched by the opt-out.
    assert ("run", 120.0) in records

    records.clear()
    _docker._flush_cleanup_registry()
    # ...and the atexit backstop honors the same unbounded opt-out.
    assert records == [("rm", None)]


# --- build_container_strategy wiring: threads the value, defaults to the const -


def test_build_container_strategy_threads_management_timeout() -> None:
    strategy = build_container_strategy(
        _FakeInner(Path("/tmp/wt")),
        image="img",
        model="m",
        management_timeout=_CUSTOM_CEILING,
    )
    assert strategy._management_timeout == _CUSTOM_CEILING  # type: ignore[attr-defined]


def test_build_container_strategy_defaults_to_module_ceiling() -> None:
    # Library callers who pass nothing stay byte-identical: the default is the
    # module constant, which equals the resolved docker-management default.
    strategy = build_container_strategy(
        _FakeInner(Path("/tmp/wt")), image="img", model="m"
    )
    assert (
        strategy._management_timeout  # type: ignore[attr-defined]
        == DEFAULT_MANAGEMENT_TIMEOUT
        == DEFAULT_DOCKER_MANAGEMENT_SECONDS
    )

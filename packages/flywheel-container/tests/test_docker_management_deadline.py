"""Held-out oracle for spec 00066 criterion #5 — docker-management deadlines.

Every docker *management* call (image/network inspect, network create, container
stop/rm, the ``atexit`` force-remove) must run under a finite, default-on,
operator-overridable wall-clock timeout, so a hung ``dockerd`` surfaces a
distinguishable docker *timeout* error within ``ceiling + epsilon`` rather than
wedging the caller while it holds the lease.

The tests:

- assert the resolved default ceiling for the docker-management class is finite
  and non-null (default-on, not a knob nobody turns on);
- point the docker primitives at a *genuinely hanging* subprocess (a real
  ``sleep`` honoring the passed timeout) and assert each management primitive
  raises ``DockerTimeoutError`` in bounded wall time;
- assert that with NO ``timeout`` argument every management primitive still hands
  ``subprocess.run`` the finite default (the gaming move criterion #5 forecloses:
  fixing one call site and leaving teardown callers passing ``None``);
- assert an operator override (a custom ceiling and the unbounded ``None``
  opt-out) flows through to ``subprocess.run``;
- assert the already-bounded ``start_container`` (120s) and ``exec_in_container``
  (governed by ``exec_timeout``) keep their existing bounds unchanged.

Do not weaken assertions.
"""

from __future__ import annotations

import inspect
import subprocess
import time
from collections.abc import Callable
from math import isfinite

import pytest

from flywheel_core.deadline_config import (
    DEFAULT_DOCKER_MANAGEMENT_SECONDS,
    DeadlineClass,
    resolve_deadlines,
)

from flywheel_container._docker import (
    DEFAULT_MANAGEMENT_TIMEOUT,
    DockerError,
    DockerTimeoutError,
    check_image_uid,
    ensure_internal_network,
    exec_in_container,
    force_remove_container_sync,
    image_exists,
    network_exists,
    remove_container,
    start_container,
)

# A tiny ceiling: a real hung subprocess must be cut off well within the test's
# wall budget. The bound is real elapsed wall-clock time, not a fake clock.
_TINY_CEILING = 0.2
# A generous upper bound on how long a single 0.2s-ceiling call may take to be
# cut off -- far above the ceiling + process-spawn slack, far below any wedge.
_WALL_BUDGET = 5.0


# Each management primitive, invoked two ways: with an explicit ``timeout`` (the
# firing path) and with NO ``timeout`` argument (the default-applies path). The
# default path is what catches a teardown caller left passing nothing.
def _with_timeout(name: str, timeout: float | None) -> Callable[[], object]:
    table: dict[str, Callable[[], object]] = {
        "image_exists": lambda: image_exists("img", timeout=timeout),
        "check_image_uid": lambda: check_image_uid("img", 1000, timeout=timeout),
        "network_exists": lambda: network_exists("net", timeout=timeout),
        "ensure_internal_network": lambda: ensure_internal_network(
            "net", timeout=timeout
        ),
        "remove_container": lambda: remove_container("box", timeout=timeout),
        "force_remove_container_sync": lambda: force_remove_container_sync(
            "box", timeout=timeout
        ),
    }
    return table[name]


def _without_timeout(name: str) -> Callable[[], object]:
    table: dict[str, Callable[[], object]] = {
        "image_exists": lambda: image_exists("img"),
        "check_image_uid": lambda: check_image_uid("img", 1000),
        "network_exists": lambda: network_exists("net"),
        "ensure_internal_network": lambda: ensure_internal_network("net"),
        "remove_container": lambda: remove_container("box"),
        "force_remove_container_sync": lambda: force_remove_container_sync("box"),
    }
    return table[name]


_MANAGEMENT_PRIMITIVES = (
    "image_exists",
    "check_image_uid",
    "network_exists",
    "ensure_internal_network",
    "remove_container",
    "force_remove_container_sync",
)


# --- default ceiling is finite + non-null (default-on) -----------------------


def test_docker_management_default_ceiling_is_non_null() -> None:
    # Default-on: with no operator override the resolved docker-management
    # ceiling is finite and non-null, and the module's applied default is that
    # same resolved value (so the management calls really are default-bounded,
    # not bounded only when an override is supplied).
    ceiling = resolve_deadlines().for_class(DeadlineClass.DOCKER_MANAGEMENT)
    assert ceiling is not None
    assert ceiling > 0
    assert isfinite(ceiling)
    assert ceiling == DEFAULT_DOCKER_MANAGEMENT_SECONDS
    assert DEFAULT_MANAGEMENT_TIMEOUT == ceiling


# --- a genuinely hung dockerd is cut off + surfaces a timeout error ----------


def _install_hanging_docker(monkeypatch: pytest.MonkeyPatch) -> None:
    """Redirect every ``subprocess.run`` in ``_docker`` at a real hanging child.

    A hung ``dockerd`` is modeled by a real ``sleep`` that outlives any
    reasonable ceiling; the passed ``timeout`` is honored verbatim, so the real
    ``subprocess.TimeoutExpired`` fires -- exactly what a wedged daemon produces.
    This proves the bound is wall-clock and that the timeout maps to a
    distinguishable docker error, not an idle/silence heuristic.
    """
    real_run = subprocess.run

    def hanging_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        timeout = kwargs.get("timeout")
        return real_run(
            ["sleep", "30"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=timeout,
        )

    monkeypatch.setattr("flywheel_container._docker.subprocess.run", hanging_run)


@pytest.mark.parametrize("name", _MANAGEMENT_PRIMITIVES)
def test_management_primitive_raises_docker_timeout_when_dockerd_hangs(
    name: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    _install_hanging_docker(monkeypatch)

    started = time.monotonic()
    with pytest.raises(DockerTimeoutError):
        _with_timeout(name, _TINY_CEILING)()
    elapsed = time.monotonic() - started

    # ceiling + epsilon, not a wedge: the call returns control in bounded wall
    # time rather than blocking on the never-returning child.
    assert elapsed < _WALL_BUDGET
    # A DockerTimeoutError is a DockerError, so existing `except DockerError`
    # handlers still contain it, but it is distinguishable as a *timeout*.
    assert issubclass(DockerTimeoutError, DockerError)


# --- the finite default applies even when the caller passes nothing ----------


def _install_recording_timeout_run(
    monkeypatch: pytest.MonkeyPatch,
) -> list[float | None]:
    """Record the ``timeout`` handed to ``subprocess.run`` and raise at once.

    Raising ``TimeoutExpired`` immediately keeps this path's wall cost ~0 even
    under the finite 120s default, while still exercising the timeout mapping;
    the recorded value proves the default reaches ``subprocess.run`` (the gaming
    move: a teardown caller left passing nothing so the timeout stays ``None``).
    """
    recorded: list[float | None] = []

    def recording_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        timeout = kwargs.get("timeout")
        recorded.append(timeout)
        raise subprocess.TimeoutExpired(
            cmd=argv, timeout=float(timeout) if timeout is not None else 0.0
        )

    monkeypatch.setattr("flywheel_container._docker.subprocess.run", recording_run)
    return recorded


@pytest.mark.parametrize("name", _MANAGEMENT_PRIMITIVES)
def test_management_primitive_default_passes_finite_timeout(
    name: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    recorded = _install_recording_timeout_run(monkeypatch)

    # No `timeout` argument -> the primitive must still bound the call with the
    # finite default; and a hung daemon under that default still raises.
    with pytest.raises(DockerTimeoutError):
        _without_timeout(name)()

    assert recorded, f"{name} never invoked subprocess.run"
    applied = recorded[0]
    assert applied is not None, f"{name} left the docker call unbounded by default"
    assert applied == DEFAULT_DOCKER_MANAGEMENT_SECONDS
    assert applied > 0
    assert isfinite(applied)


# --- an operator override (custom ceiling + unbounded opt-out) is honored -----


def _install_capturing_run(
    monkeypatch: pytest.MonkeyPatch,
) -> list[float | None]:
    """Record the ``timeout`` and return success (no timeout fires)."""
    recorded: list[float | None] = []

    def capturing_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        recorded.append(kwargs.get("timeout"))
        return subprocess.CompletedProcess(argv, 0, "id\n", "")

    monkeypatch.setattr("flywheel_container._docker.subprocess.run", capturing_run)
    return recorded


def test_operator_override_custom_ceiling_is_honored(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    recorded = _install_capturing_run(monkeypatch)
    assert image_exists("img", timeout=7.5) is True
    assert recorded == [7.5]


def test_operator_override_unbounded_opt_out_is_honored(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The unbounded opt-out (resolved to None) flows through: subprocess.run is
    # called with timeout=None, so no wall-clock bound is imposed.
    recorded = _install_capturing_run(monkeypatch)
    assert image_exists("img", timeout=None) is True
    assert recorded == [None]


def test_force_remove_override_unbounded_opt_out_is_honored(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The atexit teardown primitive honors the same opt-out.
    recorded: list[float | None] = []

    def capturing_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        recorded.append(kwargs.get("timeout"))
        return subprocess.CompletedProcess(argv, 0, "", "")

    monkeypatch.setattr("flywheel_container._docker.subprocess.run", capturing_run)
    force_remove_container_sync("box", timeout=None)
    assert recorded == [None]


# --- already-bounded paths keep their existing bounds (no regression) ---------


def test_start_container_timeout_bound_unchanged() -> None:
    # start_container keeps its existing 120s default bound; the management
    # deadline change must not regress it.
    default = inspect.signature(start_container).parameters["timeout"].default
    assert default == 120.0


def test_exec_in_container_timeout_governed_by_exec_timeout() -> None:
    # docker exec stays governed by exec_timeout (its signature default is None,
    # the caller passes the resolved exec_timeout); the management deadline must
    # not pin a bound on it here.
    default = inspect.signature(exec_in_container).parameters["timeout"].default
    assert default is None

"""The worker's startup orphan-container reap (spec 00071 #5 / D-3).

The container backend's atexit registry is a best-effort backstop for NORMAL
exit; it never runs on SIGKILL/OOM-kill, so a killed worker leaves its labelled
container alive with no reclaim path. The next worker to start scans for
flywheel-owned containers by the shared ``OWNER_LABEL_SELECTOR`` and
``docker rm -f`` each orphan.

These tests drive the reap against a faithful model of ``docker`` (no fakes of
our code): a fake ``_run_docker`` implements ``docker ps -a --filter <selector>``
exactly as documented (a ``label=KEY`` filter returns only containers carrying
that label KEY), and a fake remover records the ``docker rm -f`` targets. The
reap's real ``OWNER_LABEL_SELECTOR`` argv therefore does the selecting, so:

- a flywheel-owned container present IS reaped (the SIGKILL case the atexit
  backstop misses is actually removed — not a no-op), and
- a non-flywheel container present is NOT reaped (no collateral damage — the
  owner-label filter never returns it).

Do not weaken assertions.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence

import flywheel_container._docker as docker
from flywheel_container._docker import OWNER_LABEL, OWNER_LABEL_VALUE
from flywheel_worktree.worker import reap_container_orphans


class _FakeDocker:
    """Faithful stand-in for the ``docker`` CLI over a fixed set of containers.

    ``containers`` maps a container name to the labels it carries. ``run_docker``
    models ``docker ps -a --filter label=... --format {{.Names}}`` (the reap's
    scan) and ``remove`` models ``docker rm -f`` (the reap's removal primitive),
    recording every target so the test can assert exactly what was reaped.
    """

    def __init__(self, containers: Mapping[str, Mapping[str, str]]) -> None:
        self.containers: dict[str, dict[str, str]] = {
            name: dict(labels) for name, labels in containers.items()
        }
        self.removed: list[str] = []

    def run_docker(
        self, argv: Sequence[str], *, timeout: float | None = None
    ) -> str:
        argv = list(argv)
        assert argv[:3] == ["docker", "ps", "-a"], argv
        selector = argv[argv.index("--filter") + 1]
        assert selector.startswith("label="), selector
        key, sep, value = selector[len("label=") :].partition("=")
        matched = [
            name
            for name, labels in self.containers.items()
            if ((labels.get(key) == value) if sep else (key in labels))
        ]
        return "".join(f"{name}\n" for name in matched)

    def remove(self, name: str, *, timeout: float | None = None) -> None:
        self.removed.append(name)
        self.containers.pop(name, None)


def _install(monkeypatch, fake: _FakeDocker) -> None:
    monkeypatch.setattr(docker, "_run_docker", fake.run_docker)
    monkeypatch.setattr(docker, "force_remove_container_sync", fake.remove)


def test_reaps_flywheel_orphan(monkeypatch) -> None:
    # A flywheel-owned container left by a prior killed worker (its atexit never
    # ran) is the only signal; the startup scan must find it and issue removal.
    fake = _FakeDocker(
        {"flywheel-task-abc-0001": {OWNER_LABEL: OWNER_LABEL_VALUE}}
    )
    _install(monkeypatch, fake)

    logs: list[str] = []
    reap_container_orphans(logs.append)

    # The orphan was actually reaped -- not a no-op.
    assert fake.removed == ["flywheel-task-abc-0001"]
    assert "flywheel-task-abc-0001" not in fake.containers
    assert any("flywheel-task-abc-0001" in line for line in logs)


def test_leaves_unrelated_container(monkeypatch) -> None:
    # An unrelated, non-flywheel container present alongside a flywheel orphan:
    # only the flywheel-owned one is reaped; the unrelated container carries no
    # owner marker, so the owner-label filter never returns it -- no collateral.
    fake = _FakeDocker(
        {
            "flywheel-task-abc-0001": {OWNER_LABEL: OWNER_LABEL_VALUE},
            "team-postgres": {"com.example.role": "db"},
            "plain-container": {},
        }
    )
    _install(monkeypatch, fake)

    reap_container_orphans(lambda _msg: None)

    assert fake.removed == ["flywheel-task-abc-0001"]
    # The unrelated containers survive untouched.
    assert "team-postgres" in fake.containers
    assert "plain-container" in fake.containers


def test_no_orphan_present_is_a_clean_noop(monkeypatch) -> None:
    # No flywheel-owned container present: the reap is a clean no-op, no error.
    fake = _FakeDocker({"team-postgres": {"com.example.role": "db"}})
    _install(monkeypatch, fake)

    logs: list[str] = []
    reap_container_orphans(logs.append)

    assert fake.removed == []
    assert "team-postgres" in fake.containers


def test_active_container_is_not_reaped(monkeypatch) -> None:
    # A container a live worker in THIS process still owns (registered for
    # cleanup) must never be reaped even though it carries the owner marker;
    # only genuine orphans -- names not in the active set -- are removed.
    fake = _FakeDocker(
        {
            "flywheel-live-0001": {OWNER_LABEL: OWNER_LABEL_VALUE},
            "flywheel-orphan-0002": {OWNER_LABEL: OWNER_LABEL_VALUE},
        }
    )
    _install(monkeypatch, fake)

    reaped = docker.reap_orphan_containers(active={"flywheel-live-0001"})

    assert reaped == ["flywheel-orphan-0002"]
    assert fake.removed == ["flywheel-orphan-0002"]
    assert "flywheel-live-0001" in fake.containers

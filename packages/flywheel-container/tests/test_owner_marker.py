"""Held-out oracle for the flywheel-owner container marker (spec 00044 G5).

Every container this backend creates must carry a stable owner marker, and a
single shared selector must match *exactly* the containers that carry it — the
invariant the orphan-reap scan (reap-orphan-containers) keys on. These tests
pin the round trip purely (no daemon): the marker written at creation and the
selector read by the scan derive from one identifier, a container built with
the marker is selected, and one without it is not.

Do not weaken assertions.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence

from flywheel_container._docker import (
    OWNER_LABEL,
    OWNER_LABEL_SELECTOR,
    OWNER_LABEL_VALUE,
    build_run_argv,
    start_container,
)


# --- faithful models of docker's documented behavior (no fakes of our code) --


def _labels_from_argv(argv: Sequence[str]) -> dict[str, str]:
    """Extract the ``--label key=value`` pairs a ``docker run`` argv carries —
    i.e. the labels the resulting container would report to ``docker inspect``."""
    labels: dict[str, str] = {}
    for i, token in enumerate(argv):
        if token == "--label":
            key, _, value = argv[i + 1].partition("=")
            labels[key] = value
    return labels


def _selects(selector: str, container_labels: Mapping[str, str]) -> bool:
    """Faithful model of ``docker ps --filter <selector>`` for a label filter:
    ``label=KEY`` matches any container carrying that label KEY (any value);
    ``label=KEY=VALUE`` additionally requires the exact value."""
    assert selector.startswith("label="), selector
    key, sep, value = selector[len("label=") :].partition("=")
    if sep:
        return container_labels.get(key) == value
    return key in container_labels


# --- single source of truth -------------------------------------------------


def test_selector_and_marker_share_one_identifier() -> None:
    # The scan's selector is derived from the SAME label the marker writes, so
    # the two can never drift apart.
    assert OWNER_LABEL_SELECTOR == f"label={OWNER_LABEL}"
    assert OWNER_LABEL_SELECTOR.split("=", 1)[1] == OWNER_LABEL


def test_owner_label_is_namespaced_to_flywheel() -> None:
    # Namespacing under `flywheel.` is what lets the reaper distinguish
    # flywheel-owned containers from unrelated ones.
    assert OWNER_LABEL.startswith("flywheel.")


# --- the marker is emitted at creation --------------------------------------


def test_build_run_argv_emits_owner_label_flag() -> None:
    argv = build_run_argv("box", "img", labels={OWNER_LABEL: OWNER_LABEL_VALUE})
    assert "--label" in argv
    assert f"{OWNER_LABEL}={OWNER_LABEL_VALUE}" in argv
    # The label rides before the image, not swallowed into the command tail.
    assert argv.index("--label") < argv.index("img")


def test_start_container_always_carries_owner_marker(monkeypatch) -> None:
    captured: dict[str, list[str]] = {}

    def _capture(argv: Sequence[str], *, timeout: float | None = None) -> str:
        captured["argv"] = list(argv)
        return "container-id-abc\n"

    monkeypatch.setattr("flywheel_container._docker._run_docker", _capture)
    cid = start_container("flywheel-task-deadbeef", "img")
    assert cid == "container-id-abc"

    argv = captured["argv"]
    labels = _labels_from_argv(argv)
    assert labels.get(OWNER_LABEL) == OWNER_LABEL_VALUE
    # The container this backend creates is selected by the shared selector.
    assert _selects(OWNER_LABEL_SELECTOR, labels)


def test_start_container_owner_marker_survives_caller_labels(monkeypatch) -> None:
    captured: dict[str, list[str]] = {}

    def _capture(argv: Sequence[str], *, timeout: float | None = None) -> str:
        captured["argv"] = list(argv)
        return "cid\n"

    monkeypatch.setattr("flywheel_container._docker._run_docker", _capture)
    start_container("flywheel-task-1", "img", labels={"com.example.app": "x"})

    labels = _labels_from_argv(captured["argv"])
    # Both the owner marker and the caller's label are present; the marker is
    # not clobbered, and the selector still matches.
    assert labels.get(OWNER_LABEL) == OWNER_LABEL_VALUE
    assert labels.get("com.example.app") == "x"
    assert _selects(OWNER_LABEL_SELECTOR, labels)


# --- round trip: selected iff the marker is present -------------------------


def test_round_trip_marked_container_is_selected() -> None:
    argv = build_run_argv(
        "flywheel-box", "img", labels={OWNER_LABEL: OWNER_LABEL_VALUE}
    )
    labels = _labels_from_argv(argv)
    assert _selects(OWNER_LABEL_SELECTOR, labels)


def test_round_trip_unmarked_container_is_not_selected() -> None:
    # A container built without the owner marker (e.g. an unrelated,
    # non-flywheel container) must NOT be matched by the scan selector.
    argv = build_run_argv("some-other-box", "img")
    labels = _labels_from_argv(argv)
    assert labels == {}
    assert not _selects(OWNER_LABEL_SELECTOR, labels)


def test_round_trip_foreign_label_is_not_selected() -> None:
    # A non-flywheel container that happens to carry *some other* label is
    # still not matched — selection keys on the flywheel-owner label alone.
    argv = build_run_argv(
        "foreign-box", "img", labels={"com.example.owner": "someone-else"}
    )
    labels = _labels_from_argv(argv)
    assert not _selects(OWNER_LABEL_SELECTOR, labels)

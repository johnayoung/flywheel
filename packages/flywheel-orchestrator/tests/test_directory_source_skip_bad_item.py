"""DirectoryWorkSource skips one unloadable task file, keeps the rest.

A single malformed task file must never abort the whole listing: the source
skips it (counted in ``last_skipped_count`` and named on the ``log``), then
returns every other valid item in deterministic walk order. The skip is
*recorded*, not swallowed -- a downstream reconciler must be able to tell a
skip ("one item dropped, investigate it") from "no work" (an empty source).
"""

from __future__ import annotations

import json
from pathlib import Path

from flywheel_orchestrator import DirectoryWorkSource


def _write_task(phase: Path, task_id: str) -> None:
    phase.mkdir(parents=True, exist_ok=True)
    payload = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": "true"}],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _write_bad(phase: Path, name: str) -> Path:
    phase.mkdir(parents=True, exist_ok=True)
    bad = phase / f"{name}.json"
    bad.write_text("{ this is not valid json")
    return bad


def test_one_bad_file_is_skipped_rest_return(tmp_path: Path) -> None:
    """Exactly one malformed file among several valid ones: the result is
    exactly the valid items (bad absent, every good present)."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")
    bad = _write_bad(phase, "beta-bad")
    _write_task(phase, "gamma")

    logs: list[str] = []
    source = DirectoryWorkSource(tmp_path / "tasks", log=logs.append)
    items = source.list_work()

    ids = [item.task.id for item in items]
    # Both good items survive; an empty/partial list must fail here.
    assert ids == ["alpha", "gamma"]
    # The bad file is genuinely absent, not present-but-broken.
    assert all(item.local_path != bad for item in items)

    # The skip is recorded as a count and named on the log, not swallowed.
    assert source.last_skipped_count == 1
    assert len(logs) == 1
    assert str(bad) in logs[0]


def test_surviving_items_keep_deterministic_walk_order(tmp_path: Path) -> None:
    """The bad file between two phases does not perturb the order of the
    survivors -- they stay in filename-sorted, phase-sorted walk order."""
    p1 = tmp_path / "tasks" / "active" / "01-first"
    p2 = tmp_path / "tasks" / "active" / "02-second"
    _write_task(p1, "a-task")
    _write_bad(p1, "z-bad")
    _write_task(p1, "b-task")
    _write_task(p2, "c-task")

    source = DirectoryWorkSource(tmp_path / "tasks")
    ids = [item.task.id for item in source.list_work()]

    assert ids == ["a-task", "b-task", "c-task"]
    assert source.last_skipped_count == 1


def test_only_bad_files_yields_empty_list_plus_recorded_skips(
    tmp_path: Path,
) -> None:
    """A directory of only-bad files yields an empty list plus recorded
    skips, not a raise -- and the recorded count is what tells this apart
    from a genuinely empty source."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_bad(phase, "one-bad")
    _write_bad(phase, "two-bad")

    logs: list[str] = []
    source = DirectoryWorkSource(tmp_path / "tasks", log=logs.append)
    items = source.list_work()

    assert items == []
    assert source.last_skipped_count == 2
    assert len(logs) == 2


def test_empty_source_is_distinguishable_from_a_skip(tmp_path: Path) -> None:
    """No work at all: empty list AND zero recorded skips -- the pair a
    reconciler reads to tell 'nothing to do' from 'we dropped something'."""
    (tmp_path / "tasks" / "active").mkdir(parents=True)

    source = DirectoryWorkSource(tmp_path / "tasks")
    items = source.list_work()

    assert items == []
    assert source.last_skipped_count == 0


def test_skip_count_resets_between_calls(tmp_path: Path) -> None:
    """``last_skipped_count`` reflects only the most recent listing: once the
    bad file is removed, a re-list reports zero skips."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")
    bad = _write_bad(phase, "beta-bad")

    source = DirectoryWorkSource(tmp_path / "tasks")
    first = source.list_work()
    assert [i.task.id for i in first] == ["alpha"]
    assert source.last_skipped_count == 1

    bad.unlink()
    second = source.list_work()
    assert [i.task.id for i in second] == ["alpha"]
    assert source.last_skipped_count == 0

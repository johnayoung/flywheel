from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._workflow import collect_live_rows


def _seed_running_with_worker(
    store: SqliteStore,
    task_id: str,
    run_id: str,
    worker_id: str,
) -> Lifecycle:
    """Seed an in-flight (RUNNING) lifecycle carrying a known worker_id.

    Constructs the Lifecycle directly with the worker_id keyword before
    driving the status transitions, so the persisted row carries the
    exact seeded id.
    """
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=run_id, worker_id=worker_id)
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    store.create_lifecycle(lc)
    return lc


def test_live_snapshot_surfaces_persisted_worker_id(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running_with_worker(
            store,
            task_id="task-a",
            run_id="run-x",
            worker_id="worker-7",
        )

        rows = collect_live_rows(store)

        # The single in-flight run is the one we seeded.
        assert len(rows) == 1
        row = rows[0]
        assert row.task_id == "task-a"
        assert row.run_id == "run-x"
        assert row.status == Status.RUNNING.value

        # The criterion: the live snapshot exposes the exact persisted
        # worker_id. Fails an impl that leaves it None/"" or hardcodes a
        # different id; passes only when the seeded id is surfaced.
        assert row.worker_id == "worker-7"
    finally:
        store.close()

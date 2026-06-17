"""Held-out acceptance test: ``status --json`` surfaces an in-flight run's worker_id.

Discriminator (the one behavior this test discriminates): when the orchestrator
``status`` verb runs in ``--json`` mode against a store holding an in-flight
(``running``) lifecycle that carries a persisted non-empty ``worker_id``, the
JSON entry for that run includes the worker id as a structured VALUE -- not only
in human-rendered text, and not as malformed JSON.

The contract does NOT pin the exact key name for the new field, so this test is
deliberately KEY-AGNOSTIC: it parses stdout with ``json.loads`` (killing the
malformed-JSON gaming move), locates the seeded task's entry by its existing
stable ``task_id`` key, and asserts the seeded worker id appears as a VALUE
within that entry (searching nested dicts/lists so a nested-but-correct
placement still passes). This kills the human-text-only gaming move -- text that
never lands in the JSON has no such JSON value -- without guessing a key name.

What it FAILS:
  - worker surfaced only in the human-rendered text branch (absent from JSON);
  - worker omitted from the JSON entirely;
  - malformed / non-JSON stdout (``json.loads`` raises);
  - a hardcoded / wrong worker id (the seeded "worker-7" is not present).

What it PASSES: an impl that places the persisted "worker-7" into task-a's JSON
entry as a value under any stable key (top-level or nested).
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._workflow import main as orch_main

# The pinned example from the contract: one task with one in-flight run whose
# persisted worker is this exact id. Asserting on this literal also rejects a
# hardcoded/wrong id (e.g. an impl that always emits "" or some other worker).
_TASK_ID = "task-a"
_RUN_ID = "run-x"
_WORKER_ID = "worker-7"


def _contains_value(node: Any, target: str) -> bool:
    """True if ``target`` appears as a VALUE anywhere within ``node``.

    Searches recursively over dict values and list/tuple items so that a
    correct implementation passes whether it places the worker id at the top
    level of the entry or nests it under a sub-object. Dict KEYS are ignored
    on purpose: the contract does not pin the key name, only that the worker
    id is present as a value.
    """
    if isinstance(node, dict):
        return any(_contains_value(v, target) for v in node.values())
    if isinstance(node, (list, tuple)):
        return any(_contains_value(item, target) for item in node)
    return node == target


def test_status_json_includes_inflight_run_worker_id(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    # --- Seed a task FILE the status verb discovers under --tasks-dir. ---
    tasks_dir = tmp_path
    phase = tasks_dir / "active" / "01-phase"
    phase.mkdir(parents=True)
    (phase / f"{_TASK_ID}.json").write_text(
        json.dumps(
            {
                "id": _TASK_ID,
                "goal": "Goal for task-a.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )

    # --- Seed the in-flight (running) lifecycle carrying the worker_id into the
    # SAME db, then CLOSE the store before invoking the verb (the verb opens its
    # own store on --db). PENDING -> READY -> RUNNING leaves it in-flight. ---
    db = tasks_dir / "db.sqlite"
    store = SqliteStore(db)
    try:
        lc = Lifecycle(task_id=_TASK_ID, run_id=_RUN_ID, worker_id=_WORKER_ID)
        lc.transition_to(Status.READY)
        lc.transition_to(Status.RUNNING)
        store.create_lifecycle(lc)
    finally:
        store.close()

    # --- Invoke the status verb in --json mode as a process-style CLI. ---
    rc = orch_main(
        ["status", "--tasks-dir", str(tasks_dir), "--db", str(db), "--json"]
    )
    assert rc == 0

    # --- Valid-JSON confirmed by parsing; structure asserted, never a substring. ---
    out = capsys.readouterr().out
    payload = json.loads(out)
    assert isinstance(payload, list)

    # Locate the seeded task's entry by its existing stable "task_id" key.
    entry = next(e for e in payload if e["task_id"] == _TASK_ID)

    # Sanity-pin the example: this entry describes the in-flight run we seeded.
    assert entry["latest_run_id"] == _RUN_ID
    assert entry["latest_status"] == "running"

    # The discriminator: the persisted worker id is present as a VALUE within
    # the entry (top-level or nested) -- key-agnostic, structural, not a raw
    # stdout substring. Fails human-text-only / omitted-from-JSON / wrong-id;
    # malformed JSON already failed json.loads above.
    assert _contains_value(entry, _WORKER_ID)

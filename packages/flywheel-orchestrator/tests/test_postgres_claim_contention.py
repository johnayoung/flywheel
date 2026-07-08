"""No task is double-claimed across concurrent postgres claim-store instances
(spec 00075).

Both tests build their claim stores through the *production* construction point
(:func:`build_claim_store` with a postgres policy), so they also prove the
factory routes the claim store onto postgres and writes no sqlite file. Two
distinct store instances (each its own connection pool) model two concurrent
worker processes contending on one schema.

* ``test_two_postgres_claimants_split_every_task_exactly_once`` serializes the
  two claimants in a deterministic, lock-stepped baton: for every task both
  claimants attempt the acquire, with an alternating first mover. The acquire
  path must grant each task to exactly one claimant and refuse the peer
  (returns ``None``). Each claimant wins a disjoint half, so both obtain at
  least one claim and every task is claimed exactly once. A broken acquire that
  let the peer steal a live lease would return a second claim and fail here.

* ``test_two_postgres_claimants_racing_one_task_yield_one_winner`` lets both
  claimants hit the *same* task at the same instant (a two-way barrier per
  task) under genuine concurrency; exactly one must win and the other be
  refused, for every task.

Requires a reachable Postgres (Docker / testcontainers / the ``postgres``
extra); skipped only when no database is reachable.
"""

from __future__ import annotations

import threading
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

from flywheel_orchestrator import PostgresClaimStore
from flywheel_orchestrator._policy import WorkPolicy
from flywheel_orchestrator._store_factory import PG_DSN_ENV, build_claim_store


def _policy(schema: str) -> WorkPolicy:
    return WorkPolicy(
        source_kind="directory",
        tasks_dir=Path(".flywheel/tasks"),
        store_backend="postgres",
        store_schema=schema,
    )


def _now() -> datetime:
    return datetime(2026, 5, 28, 12, 0, 0, tzinfo=timezone.utc)


_LEASE_SECONDS = 3600.0


def test_two_postgres_claimants_split_every_task_exactly_once(
    tmp_path: Path,
    monkeypatch,
    require_postgres: str,
) -> None:
    dsn = require_postgres
    monkeypatch.setenv(PG_DSN_ENV, dsn)
    schema = f"flywheel_contention_{uuid4().hex[:12]}"
    policy = _policy(schema)
    db_path = tmp_path / "flywheel.sqlite"

    # Production construction point: a postgres policy routes both instances
    # onto postgres and creates no sqlite file.
    store_a = build_claim_store(policy, db_path=db_path)
    store_b = build_claim_store(policy, db_path=db_path)
    assert isinstance(store_a, PostgresClaimStore)
    assert isinstance(store_b, PostgresClaimStore)
    assert not db_path.exists()

    task_ids = [f"t{i}" for i in range(6)]
    total_steps = 2 * len(task_ids)

    cond = threading.Condition()
    state = {"step": 0}
    outcomes: dict[int, object] = {}
    errors: list[BaseException] = []

    def _drive(worker_id: str, store: PostgresClaimStore, steps: list[int]) -> None:
        for step in steps:
            with cond:
                reached = True
                while state["step"] != step:
                    if not cond.wait(timeout=10):
                        reached = False
                        break
                if not reached:
                    errors.append(TimeoutError(f"step {step} never reached"))
                    return
                task_id = task_ids[step // 2]
                try:
                    outcomes[step] = store.acquire_claim(
                        task_id,
                        worker_id,
                        now=_now(),
                        lease_seconds=_LEASE_SECONDS,
                    )
                except BaseException as exc:  # noqa: BLE001 - surfaced via errors
                    errors.append(exc)
                    outcomes[step] = exc
                state["step"] += 1
                cond.notify_all()

    # ``worker-a`` owns steps where (round parity) == (sub-step); ``worker-b``
    # owns the rest. That makes A the first mover -- and winner -- on even
    # tasks and B on odd tasks, while the peer always contends the just-granted
    # live claim and must be refused. Together the two step lists partition
    # every step exactly once, so the shared counter always advances.
    a_steps = [s for s in range(total_steps) if (s // 2) % 2 == s % 2]
    b_steps = [s for s in range(total_steps) if (s // 2) % 2 != s % 2]

    try:
        ta = threading.Thread(
            target=_drive, args=("worker-a", store_a, a_steps)
        )
        tb = threading.Thread(
            target=_drive, args=("worker-b", store_b, b_steps)
        )
        ta.start()
        tb.start()
        ta.join(30)
        tb.join(30)
        assert not ta.is_alive() and not tb.is_alive()
        assert not errors, f"claimants raised: {errors!r}"

        winners: dict[str, str] = {}
        for r, task_id in enumerate(task_ids):
            first = outcomes[2 * r]  # the first mover must win
            peer = outcomes[2 * r + 1]  # the peer must be refused
            assert first is not None, f"{task_id}: the first mover was refused"
            assert peer is None, (
                f"{task_id}: the peer double-claimed a live lease"
            )
            winners[task_id] = first.worker_id  # type: ignore[attr-defined]

        won_a = {t for t, w in winners.items() if w == "worker-a"}
        won_b = {t for t, w in winners.items() if w == "worker-b"}
        # Each claimant obtained at least one claim...
        assert won_a, "worker-a obtained no claim"
        assert won_b, "worker-b obtained no claim"
        # ...the winners are disjoint and cover every task exactly once...
        assert won_a.isdisjoint(won_b)
        assert won_a | won_b == set(task_ids)
        # ...and the schema holds exactly one live claim per task.
        assert len(store_a.list_claims()) == len(task_ids)
    finally:
        store_a.close()
        store_b.close()


def test_two_postgres_claimants_racing_one_task_yield_one_winner(
    tmp_path: Path,
    monkeypatch,
    require_postgres: str,
) -> None:
    dsn = require_postgres
    monkeypatch.setenv(PG_DSN_ENV, dsn)
    schema = f"flywheel_race_{uuid4().hex[:12]}"
    policy = _policy(schema)
    db_path = tmp_path / "flywheel.sqlite"

    store_a = build_claim_store(policy, db_path=db_path)
    store_b = build_claim_store(policy, db_path=db_path)
    assert isinstance(store_a, PostgresClaimStore)
    assert isinstance(store_b, PostgresClaimStore)
    assert not db_path.exists()

    task_ids = [f"t{i}" for i in range(6)]

    try:
        for task_id in task_ids:
            gate = threading.Barrier(2)
            results: dict[str, object] = {}

            def _race(
                worker_id: str,
                store: PostgresClaimStore,
                tid: str = task_id,
            ) -> None:
                gate.wait(timeout=5)
                try:
                    results[worker_id] = store.acquire_claim(
                        tid,
                        worker_id,
                        now=_now(),
                        lease_seconds=_LEASE_SECONDS,
                    )
                except BaseException as exc:  # noqa: BLE001
                    results[worker_id] = exc

            ta = threading.Thread(
                target=_race, args=("worker-a", store_a)
            )
            tb = threading.Thread(
                target=_race, args=("worker-b", store_b)
            )
            ta.start()
            tb.start()
            ta.join(10)
            tb.join(10)
            assert not ta.is_alive() and not tb.is_alive()

            outcomes = [results["worker-a"], results["worker-b"]]
            assert all(not isinstance(o, Exception) for o in outcomes), outcomes
            winners = [o for o in outcomes if o is not None]
            refused = [o for o in outcomes if o is None]
            # Exactly one claimant wins the contested task; the peer is refused.
            assert len(winners) == 1, f"{task_id}: {len(winners)} winners"
            assert len(refused) == 1, f"{task_id}: {len(refused)} refusals"

        # One live claim per task, none double-claimed.
        assert len(store_a.list_claims()) == len(task_ids)
    finally:
        store_a.close()
        store_b.close()

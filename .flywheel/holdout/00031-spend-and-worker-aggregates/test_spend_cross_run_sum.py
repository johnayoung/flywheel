"""Held-out acceptance test: summarize_spend is a true CROSS-RUN rollup.

Authored blind from the spec-00031 contract. The single behavior under test:
``store.summarize_spend()`` (no time bound) returns the sum of every attempt's
token and cost columns across EVERY run in the store -- not one run, not the
latest run, not a constant.

The fixture parametrizes over all three backends, so one assertion of the
cross-run sum simultaneously proves cross-backend identity: a backend that omits
or diverges on ``summarize_spend`` fails its own arm. Postgres SKIPS (never
fails) when no database is reachable, mirroring the committed contract suite.
"""

from __future__ import annotations

from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

import pytest

from flywheel_core import (
    Attempt,
    InMemoryStore,
    Lifecycle,
    SpendSummary,
    SqliteStore,
)

_STORE_BACKENDS = ("memory", "sqlite", "postgres")


@pytest.fixture(params=_STORE_BACKENDS, ids=_STORE_BACKENDS)
def store(
    request: pytest.FixtureRequest,
    tmp_path: Path,
    postgres_dsn: str | None,
) -> Iterator[object]:
    param = request.param
    if param == "memory":
        instance: object = InMemoryStore()
    elif param == "sqlite":
        instance = SqliteStore(tmp_path / "contract.db")
    else:
        if postgres_dsn is None:
            pytest.skip("Postgres backend skipped: no database reachable")
        from flywheel_core import PostgresStore

        instance = PostgresStore(
            postgres_dsn,
            schema=f"flywheel_test_{uuid4().hex[:12]}",
            pool_min=1,
            pool_max=4,
        )
    try:
        yield instance
    finally:
        close = getattr(instance, "close", None)
        if callable(close):
            close()


def _seed_run(
    store: object,
    run_id: str,
    *,
    input_tokens: int,
    output_tokens: int,
    cache_creation_input_tokens: int,
    cache_read_input_tokens: int,
    total_cost_usd: float,
) -> None:
    """Create a parent lifecycle row, then one attempt carrying the spend."""
    store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))  # type: ignore[attr-defined]
    store.save_attempt(  # type: ignore[attr-defined]
        run_id,
        Attempt(
            number=1,
            started_at=datetime(2026, 1, 1, tzinfo=timezone.utc),
            run_id=run_id,
            input_tokens=input_tokens,
            output_tokens=output_tokens,
            cache_creation_input_tokens=cache_creation_input_tokens,
            cache_read_input_tokens=cache_read_input_tokens,
            total_cost_usd=total_cost_usd,
        ),
    )


def test_summarize_spend_sums_every_attempt_across_runs(store: object) -> None:
    # Pinned example from the contract. Run A and run B carry distinct,
    # non-round token/cost values chosen so that NO single run's totals equal
    # the grand total (kills single-run / latest-run fakes) and the grand cost
    # is not a round number (kills a hardcoded constant).
    run_a = f"run-a-{uuid4().hex[:8]}"
    run_b = f"run-b-{uuid4().hex[:8]}"

    a = dict(
        input_tokens=1000,
        output_tokens=200,
        cache_creation_input_tokens=30,
        cache_read_input_tokens=4,
        total_cost_usd=0.125,
    )
    b = dict(
        input_tokens=70,
        output_tokens=11,
        cache_creation_input_tokens=5,
        cache_read_input_tokens=2,
        total_cost_usd=0.0037,
    )

    _seed_run(store, run_a, **a)  # type: ignore[arg-type]
    _seed_run(store, run_b, **b)  # type: ignore[arg-type]

    # Expected grand totals computed from the seeded values -- never read off
    # the implementation. Source columns carry the '_input' infix; the summary
    # fields drop it (mapped by position).
    expected_input_tokens = a["input_tokens"] + b["input_tokens"]
    expected_output_tokens = a["output_tokens"] + b["output_tokens"]
    expected_cache_creation_tokens = (
        a["cache_creation_input_tokens"] + b["cache_creation_input_tokens"]
    )
    expected_cache_read_tokens = (
        a["cache_read_input_tokens"] + b["cache_read_input_tokens"]
    )
    expected_total_cost_usd = a["total_cost_usd"] + b["total_cost_usd"]

    # Sanity: the pinned example's grand totals match the contract's numbers,
    # neither single run equals the grand total, and the cost is not round.
    assert expected_input_tokens == 1070
    assert expected_output_tokens == 211
    assert expected_cache_creation_tokens == 35
    assert expected_cache_read_tokens == 6
    assert abs(expected_total_cost_usd - 0.1287) < 1e-9
    assert a["input_tokens"] != expected_input_tokens
    assert b["input_tokens"] != expected_input_tokens

    summary = store.summarize_spend()  # type: ignore[attr-defined]

    assert isinstance(summary, SpendSummary)
    assert summary.input_tokens == expected_input_tokens
    assert summary.output_tokens == expected_output_tokens
    assert summary.cache_creation_tokens == expected_cache_creation_tokens
    assert summary.cache_read_tokens == expected_cache_read_tokens
    assert abs(summary.total_cost_usd - expected_total_cost_usd) < 1e-9

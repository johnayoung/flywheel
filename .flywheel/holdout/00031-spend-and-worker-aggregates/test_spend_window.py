"""Held-out acceptance test for the half-open spend-aggregate window.

Authored blind to the implementation. Asserts ONLY the declared observable
contract of ``store.summarize_spend(since=..., until=...)``:

  * the window is HALF-OPEN ``[since, until)`` measured against each attempt's
    ``last_activity_at``: ``last_activity_at == since`` is INCLUDED,
    ``last_activity_at == until`` is EXCLUDED;
  * only in-window attempts contribute to the five ``SpendSummary`` sums;
  * an empty window returns the all-zero summary, never the grand total.

These cases jointly kill the cheapest fakes: ignoring the window and returning
the all-time grand total, and an off-by-one on either half-open boundary.
"""

from __future__ import annotations

from collections.abc import Iterator
from datetime import datetime, timedelta, timezone
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


# Three distinct activity timestamps: T_before < T_in < T_after.
_T_BEFORE = datetime(2026, 1, 1, 0, 0, 0, tzinfo=timezone.utc)
_T_IN = datetime(2026, 1, 2, 0, 0, 0, tzinfo=timezone.utc)
_T_AFTER = datetime(2026, 1, 3, 0, 0, 0, tzinfo=timezone.utc)

# Per-attempt SOURCE values. The in-window attempt has the known target values;
# the others are DIFFERENT nonzero values so a window-ignoring impl must return
# a strictly larger (and unequal) grand total.
_VALUES_BEFORE = dict(
    input_tokens=100,
    output_tokens=10,
    cache_creation_input_tokens=2,
    cache_read_input_tokens=1,
    total_cost_usd=0.011,
)
_VALUES_IN = dict(
    input_tokens=500,
    output_tokens=40,
    cache_creation_input_tokens=7,
    cache_read_input_tokens=3,
    total_cost_usd=0.061,
)
_VALUES_AFTER = dict(
    input_tokens=200,
    output_tokens=20,
    cache_creation_input_tokens=4,
    cache_read_input_tokens=5,
    total_cost_usd=0.029,
)


def _seed(store: object, *, last_activity_at: datetime, values: dict) -> None:
    """Write one run + parent lifecycle + one attempt at ``last_activity_at``."""
    run_id = uuid4().hex
    store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))  # type: ignore[attr-defined]
    store.save_attempt(  # type: ignore[attr-defined]
        run_id,
        Attempt(
            number=1,
            started_at=last_activity_at,
            run_id=run_id,
            last_activity_at=last_activity_at,
            **values,
        ),
    )


def _assert_equals_values(summary: SpendSummary, values: dict) -> None:
    """``summary`` carries exactly ``values`` (cache fields drop ``_input``)."""
    assert summary.input_tokens == values["input_tokens"]
    assert summary.output_tokens == values["output_tokens"]
    assert summary.cache_creation_tokens == values["cache_creation_input_tokens"]
    assert summary.cache_read_tokens == values["cache_read_input_tokens"]
    assert summary.total_cost_usd == pytest.approx(values["total_cost_usd"])


def _assert_all_zero(summary: SpendSummary) -> None:
    assert summary.input_tokens == 0
    assert summary.output_tokens == 0
    assert summary.cache_creation_tokens == 0
    assert summary.cache_read_tokens == 0
    assert summary.total_cost_usd == 0.0


def test_half_open_window_returns_only_in_window_attempt(store: object) -> None:
    """[T_in, T_after) selects exactly the T_in attempt.

    T_before is excluded (before the closed lower bound), T_after is excluded
    (the open upper bound), T_in is included (closed lower bound). The bounded
    result must equal ONLY the in-window attempt and must STRICTLY DIFFER from
    the unbounded grand total, which kills a window-ignoring fake.
    """
    _seed(store, last_activity_at=_T_BEFORE, values=_VALUES_BEFORE)
    _seed(store, last_activity_at=_T_IN, values=_VALUES_IN)
    _seed(store, last_activity_at=_T_AFTER, values=_VALUES_AFTER)

    bounded = store.summarize_spend(since=_T_IN, until=_T_AFTER)  # type: ignore[attr-defined]
    _assert_equals_values(bounded, _VALUES_IN)

    # Metamorphic contrast: the unbounded total sums all three and must differ.
    grand = store.summarize_spend()  # type: ignore[attr-defined]
    assert grand.input_tokens == (
        _VALUES_BEFORE["input_tokens"]
        + _VALUES_IN["input_tokens"]
        + _VALUES_AFTER["input_tokens"]
    )
    assert bounded.input_tokens != grand.input_tokens
    assert bounded != grand


def test_empty_window_returns_all_zero_not_grand_total(store: object) -> None:
    """A window containing no attempt returns the all-zero summary."""
    _seed(store, last_activity_at=_T_BEFORE, values=_VALUES_BEFORE)
    _seed(store, last_activity_at=_T_IN, values=_VALUES_IN)
    _seed(store, last_activity_at=_T_AFTER, values=_VALUES_AFTER)

    empty = store.summarize_spend(  # type: ignore[attr-defined]
        since=_T_AFTER + timedelta(days=1),
        until=_T_AFTER + timedelta(days=2),
    )
    _assert_all_zero(empty)


def test_lower_bound_is_closed_attempt_at_since_is_included(store: object) -> None:
    """last_activity_at == since IS counted (kills ``>`` vs ``>=`` lower bound)."""
    _seed(store, last_activity_at=_T_IN, values=_VALUES_IN)

    # Window opens exactly on the only attempt's timestamp; it must be inside.
    at_since = store.summarize_spend(since=_T_IN, until=_T_AFTER)  # type: ignore[attr-defined]
    _assert_equals_values(at_since, _VALUES_IN)


def test_upper_bound_is_open_attempt_at_until_is_excluded(store: object) -> None:
    """last_activity_at == until is NOT counted (kills ``>=`` vs ``>`` upper bound)."""
    _seed(store, last_activity_at=_T_IN, values=_VALUES_IN)

    # Window closes exactly on the only attempt's timestamp; it must be outside.
    at_until = store.summarize_spend(since=_T_BEFORE, until=_T_IN)  # type: ignore[attr-defined]
    _assert_all_zero(at_until)

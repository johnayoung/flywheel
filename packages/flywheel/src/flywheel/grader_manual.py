"""Manual grader gate-selection and receipt assembly.

Pure module that supports the human-approval gate described in
``.flywheel/specs/00016-FEATURE-manual-grader-approval-gate.md``. The
harness/resolver layer drives IO; this module only:

* Selects the next pending :class:`flywheel.task.ManualGrader` on a
  :class:`flywheel.task.Task` as a small :class:`ManualGate` snapshot
  preserving the grader's index in ``task.graders`` as its ordinal.
* Assembles a :class:`GraderResultRecord` for an approve or reject
  decision so the resolver can append it via the store contract.

Manual graders do not run a subprocess and have no live attempt clock
of their own (the attempt is already finalized ``SUCCEEDED`` by the
time the gate is reached, per the spec's ``SUCCEEDED`` semantics), so
``duration_ms`` is always ``0`` on manual receipts.

The module deliberately imports no ``json`` / ``pathlib`` / ``io`` /
``claude`` SDK — it is pure data plus record assembly and is safe to
import from any other pure module.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from typing import Any

from flywheel.store_protocols import GraderResultRecord
from flywheel.task import ManualGrader, Task


@dataclass(frozen=True, kw_only=True)
class ManualGate:
    """A pending manual-approval gate snapshot.

    ``ordinal`` is the gate's index in ``task.graders`` (preserved
    across non-manual neighbors so audits can reconstruct the original
    list from receipts alone). ``instruction`` is the operator-facing
    instruction text from the underlying :class:`ManualGrader`.
    ``grader_name`` is the grader's optional human label.
    """

    ordinal: int
    instruction: str
    grader_name: str | None


def next_pending_manual_gate(
    task: Task,
    *,
    after_ordinal: int | None,
) -> ManualGate | None:
    """Return the first manual gate in ``task.graders`` strictly after
    ``after_ordinal``.

    When ``after_ordinal`` is ``None`` the search returns the first
    manual gate at any index. Non-manual graders are skipped while
    their indices stay aligned to ``task.graders`` — the returned
    :attr:`ManualGate.ordinal` is the grader's literal position in the
    list, not the count of manual graders preceding it.

    Returns ``None`` when no manual gate remains.
    """

    for ordinal, grader in enumerate(task.graders):
        if not isinstance(grader, ManualGrader):
            continue
        if after_ordinal is not None and ordinal <= after_ordinal:
            continue
        return ManualGate(
            ordinal=ordinal,
            instruction=grader.instruction,
            grader_name=grader.name,
        )
    return None


def _grader_spec_snapshot(gate: ManualGate) -> dict[str, Any]:
    """Snapshot the gate's grader as it appeared in the task at gate time."""

    spec: dict[str, Any] = {
        "type": "manual",
        "instruction": gate.instruction,
    }
    if gate.grader_name is not None:
        spec["name"] = gate.grader_name
    return spec


def build_manual_result(
    gate: ManualGate,
    *,
    run_id: str,
    attempt_number: int,
    passed: bool,
    summary: str,
    now: datetime,
) -> GraderResultRecord:
    """Assemble a ``grader_type="manual"`` :class:`GraderResultRecord`.

    The record is built but not persisted; the caller (the harness /
    resolver) appends it via :meth:`GraderResultStore.append_grader_result`.

    ``passed=True`` represents an operator approve, ``passed=False`` an
    operator reject. ``summary`` carries the operator feedback verbatim
    (or the ``"(no feedback provided)"`` placeholder the resolver
    substitutes when the reject payload omits feedback). ``now`` is
    injected so the receipt's timestamp matches the lifecycle event's
    timestamp deterministically.
    """

    payload: dict[str, Any] = {
        "instruction": gate.instruction,
        "summary": summary,
    }
    return GraderResultRecord(
        run_id=run_id,
        attempt_number=attempt_number,
        ordinal=gate.ordinal,
        grader_type="manual",
        grader_spec=_grader_spec_snapshot(gate),
        grader_name=gate.grader_name,
        passed=passed,
        duration_ms=0,
        payload=payload,
        ts=now,
    )


__all__ = [
    "ManualGate",
    "build_manual_result",
    "next_pending_manual_gate",
]

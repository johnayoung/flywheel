"""Held-out landing gate engine (spec 00050, decisions D-2..D-5).

The orchestrator-owned half of held-out gating: grade an agent's *committed*
result with **operator-declared command graders the agent never saw**. This
module is the gate *engine* — it loads a task's held-out graders from an
operator-owned source the orchestrator reads, runs them against the committed
tree out-of-band (reusing :func:`flywheel_core.grader_command.run_command_graders`,
the same runner the harness uses), and computes a pass/fail landing verdict.
Wiring the verdict into the per-task drive (parking, outcome recording) is the
landing-integration concern and lives elsewhere; this module produces the
verdict only.

Invariants this engine enforces (and the spec criteria they defend):

* **Out-of-band, never agent-facing (D-2, #3).** Held-out graders live in a
  source keyed by task id that the orchestrator reads. They are *not* merged
  into the ``Task`` handed to the harness and are never written into the
  agent's worktree. The engine reads them from the source at gate time; the
  agent-facing ``Task`` is untouched.
* **The verdict is the grader exit code, never the agent's self-report (D-4,
  #4).** The engine does not look at the agent's terminal status at all. An
  agent that reports DONE while a held-out grader exits non-zero yields a FAIL.
* **Fail closed (D-3, #5).** A registered held-out grader that cannot be
  located or executed yields FAIL — never a pass, never a silent skip. A task
  with *no* registered held-out graders yields a distinct ``NO_GATE`` verdict
  so the integration layer lands it unchanged.
* **Command graders only (D-5).** Held-out graders are command graders
  evaluated against the committed tree. A non-command held-out registration is
  a misconfiguration and fails closed.
* **Graded against the committed result (#9).** The committed tree is passed as
  ``cwd`` to every held-out grader, so each observes the agent's committed
  changes — never a stale or pre-run tree.
"""

from __future__ import annotations

import json
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from pathlib import Path
from typing import Protocol, runtime_checkable

from flywheel_core.grader_command import run_command_graders
from flywheel_core.loaders import TaskLoadError, load_graders
from flywheel_core.store_protocols import GraderResultRecord
from flywheel_core.task import CommandGrader, Task


class HeldOutGraderError(Exception):
    """A held-out registration exists for a task but cannot be loaded.

    Raised by a :class:`HeldOutGraderSource` when it finds a registration keyed
    to the task id that it cannot turn into runnable command graders (missing
    file content, malformed JSON, a non-command grader, an empty registration).
    The engine maps this to a fail-closed ``FAIL`` verdict — the absence of a
    *parseable* held-out check is never treated as the absence of a check.
    """


class GateOutcome(str, Enum):
    """The three terminal states of a held-out landing gate.

    ``NO_GATE`` is deliberately distinct from ``FAIL``: a task that never
    registered a held-out grader must land byte-identically to today (D-7),
    whereas a task whose registered grader is unrunnable must be blocked (D-3).
    """

    NO_GATE = "no_gate"
    PASS = "pass"
    FAIL = "fail"


@dataclass(frozen=True)
class GateVerdict:
    """The engine's pass/fail landing verdict for one task.

    ``outcome`` is authoritative; ``reason`` is an operator-readable summary
    suitable for the recorded gate-failed outcome (D-6, criterion #6).
    ``results`` carries the held-out grader receipts in execution order for the
    integration layer to persist or surface — empty for ``NO_GATE`` and for a
    load-time fail-closed (no grader ran).
    """

    outcome: GateOutcome
    reason: str
    results: tuple[GraderResultRecord, ...] = field(default_factory=tuple)

    @property
    def passed(self) -> bool:
        """True only when every registered held-out grader ran and passed."""
        return self.outcome is GateOutcome.PASS

    @property
    def blocks_landing(self) -> bool:
        """True when the gate must block the land (any fail / fail-closed).

        ``NO_GATE`` does not block — the integration layer lands such a task
        unchanged.
        """
        return self.outcome is GateOutcome.FAIL


@runtime_checkable
class HeldOutGraderSource(Protocol):
    """Operator-owned source of held-out command graders, keyed by task id.

    The contract the engine relies on (D-2):

    * ``graders_for(task_id)`` returns the registered held-out command graders
      for the task, or ``None`` when the task has *no* registration (the
      ``NO_GATE`` path).
    * It raises :class:`HeldOutGraderError` when a registration exists but
      cannot be turned into runnable command graders (the fail-closed path).

    Implementations MUST NOT expose held-out graders through the agent-facing
    ``Task`` or write them into the agent's worktree; the source is a side
    channel the orchestrator reads at gate time.
    """

    def graders_for(self, task_id: str) -> list[CommandGrader] | None: ...


@dataclass(frozen=True)
class FilesystemHeldOutGraderSource:
    """Held-out graders declared on disk under ``root``, one file per task id.

    Layout: ``<root>/<task_id>.json`` holds the task's held-out command
    graders, either as a bare JSON list of grader entries or as an object with
    a ``graders`` key — the same per-entry shape as a ``Task``'s ``graders``
    field (reusing :func:`flywheel_core.loaders.load_graders` for validation).
    The directory lives outside any agent worktree; the orchestrator points the
    source at it, the agent never receives the path.

    Fail-closed posture (D-3):

    * No file for the task id -> ``None`` (NO_GATE; the task never opted in).
    * File present but unreadable / malformed / containing a non-command grader
      / declaring zero graders -> :class:`HeldOutGraderError` (blocks the land).
    """

    root: Path

    def graders_for(self, task_id: str) -> list[CommandGrader] | None:
        path = self._path_for(task_id)
        if path is None or not path.is_file():
            return None
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as exc:
            raise HeldOutGraderError(
                f"{path}: held-out grader registration cannot be read: {exc}"
            ) from exc
        try:
            data = json.loads(text)
        except json.JSONDecodeError as exc:
            raise HeldOutGraderError(
                f"{path}: held-out grader registration is invalid JSON: {exc}"
            ) from exc

        if isinstance(data, Mapping):
            entries = data.get("graders")
        else:
            entries = data
        try:
            graders = load_graders(entries, source=str(path))
        except TaskLoadError as exc:
            raise HeldOutGraderError(
                f"held-out grader registration could not be parsed: {exc}"
            ) from exc

        command = [g for g in graders if isinstance(g, CommandGrader)]
        if len(command) != len(graders):
            raise HeldOutGraderError(
                f"{path}: held-out graders must be command graders only "
                f"(D-5); found a non-command grader"
            )
        if not command:
            raise HeldOutGraderError(
                f"{path}: held-out grader registration declares no command "
                f"graders; refusing to certify an empty gate as passing"
            )
        return command

    def _path_for(self, task_id: str) -> Path | None:
        """Resolve ``<root>/<task_id>.json``, rejecting path traversal.

        A task id that escapes ``root`` (separators, ``..``) is treated as
        unregistered rather than allowed to read an arbitrary file.
        """
        if not task_id or "/" in task_id or "\\" in task_id or task_id in (
            ".",
            "..",
        ):
            return None
        candidate = (self.root / f"{task_id}.json").resolve()
        root = self.root.resolve()
        if root != candidate and root not in candidate.parents:
            return None
        return candidate


def evaluate_held_out_gate(
    task: Task,
    source: HeldOutGraderSource,
    *,
    committed_tree: str | Path,
    run_id: str,
    env: Mapping[str, str] | None = None,
    per_grader_timeout_seconds: float | None = None,
    now: Callable[[], datetime] | None = None,
) -> GateVerdict:
    """Compute the held-out landing verdict for ``task``'s committed result.

    Loads ``task``'s held-out command graders from ``source`` and runs them
    against ``committed_tree`` (the agent's committed working tree) out of the
    agent's turn, reusing the core command-grader runner. The verdict is the
    out-of-band exit code, never ``task``'s reported status (D-4): this function
    is not given the agent's terminal status and does not consult it.

    Returns:

    * ``NO_GATE`` — no held-out graders registered for the task; land unchanged.
    * ``PASS`` — every registered held-out grader ran and exited zero.
    * ``FAIL`` — at least one held-out grader exited non-zero, OR the
      registration / runner could not be executed (fail closed, D-3).
    """
    try:
        held_out = source.graders_for(task.id)
    except HeldOutGraderError as exc:
        return GateVerdict(
            GateOutcome.FAIL,
            f"held-out gate failed closed: {exc}",
        )

    if not held_out:
        return GateVerdict(
            GateOutcome.NO_GATE,
            "no held-out graders registered for this task",
        )

    gate_task = Task(
        goal=f"held-out landing gate for {task.id}",
        graders=list(held_out),
        id=task.id,
    )

    recorder = _GateGraderRecorder()
    try:
        records = run_command_graders(
            gate_task,
            recorder,
            run_id=run_id,
            # Held-out gate checks sit outside the lifecycle's attempt
            # numbering; the engine returns receipts, it does not persist
            # lifecycle history.
            attempt_number=0,
            cwd=committed_tree,
            env=env,
            per_grader_timeout_seconds=per_grader_timeout_seconds,
            now=now,
        )
    except Exception as exc:  # noqa: BLE001 - fail closed on any runner error
        # The runner itself could not run (e.g. the committed tree is missing).
        # An unrunnable held-out check is a FAIL, never a skip (D-3).
        return GateVerdict(
            GateOutcome.FAIL,
            f"held-out gate failed closed: grader runner errored "
            f"({type(exc).__name__}: {exc})",
        )

    expected = len(held_out)
    ran_all = len(records) == expected
    all_passed = all(record.passed for record in records)

    if ran_all and all_passed:
        return GateVerdict(
            GateOutcome.PASS,
            f"all {expected} held-out grader(s) passed against the "
            f"committed result",
            tuple(records),
        )

    return GateVerdict(
        GateOutcome.FAIL,
        _fail_reason(records, expected),
        tuple(records),
    )


def _fail_reason(records: tuple[GraderResultRecord, ...] | list[GraderResultRecord], expected: int) -> str:
    """Summarize why a held-out gate blocked the land, for the recorded outcome.

    ``run_command_graders`` stops at the first failing grader, so the failing
    grader is the last record and later registered graders never ran; both are
    reported so an operator sees the gate caught something rather than a clean
    DONE-land (criterion #6).
    """
    failed = [r for r in records if not r.passed]
    ran = len(records)
    parts: list[str] = [
        f"held-out gate FAILED: {ran} of {expected} held-out grader(s) ran"
    ]
    for record in failed:
        label = record.grader_name or str(record.grader_spec.get("run", ""))
        exit_code = record.payload.get("exit_code")
        termination = record.payload.get("termination", "exited")
        parts.append(
            f"[{label}] {termination} exit_code={exit_code}"
        )
    if ran < expected:
        parts.append(
            f"{expected - ran} held-out grader(s) did not run after the "
            f"first failure"
        )
    return "; ".join(parts)


class _GateGraderRecorder:
    """In-memory ``GraderResultStore`` for held-out gate runs.

    Held-out gate receipts are a landing-decision signal, not lifecycle
    history: the run's authoritative receipts were persisted in-run under the
    store's ``(run_id, attempt_number, ordinal)`` key, which has no slot for an
    out-of-band gate re-check. The engine returns the receipts in its verdict;
    persisting them (if at all) is the integration layer's choice. Mirrors the
    worktree submitter's ``_ReverifyRecorder``.
    """

    def __init__(self) -> None:
        self.records: list[GraderResultRecord] = []

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        self.records.append(result)
        return result

    def list_grader_results(
        self, run_id: str, attempt_number: int
    ) -> list[GraderResultRecord]:
        return [
            r
            for r in self.records
            if r.run_id == run_id and r.attempt_number == attempt_number
        ]


__all__ = [
    "FilesystemHeldOutGraderSource",
    "GateOutcome",
    "GateVerdict",
    "HeldOutGraderError",
    "HeldOutGraderSource",
    "evaluate_held_out_gate",
]

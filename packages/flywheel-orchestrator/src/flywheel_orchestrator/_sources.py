"""Work sources — where the orchestrator's work items come from.

The orchestrator schedules :class:`WorkItem` values; it does not care where
they live. A :class:`WorkSource` is the inbound/outbound boundary with
whatever system owns the work — a directory of task JSON files, an issue
tracker, a queue. Everything tool-specific stays in the adapter:

* **Inbound** — :meth:`WorkSource.list_work` enumerates the items that are
  candidates right now, each already compiled to a validated
  :class:`~flywheel_core.task.Task` (plus the orchestration-layer
  ``prerequisites`` edges). Items that cannot compile to a runnable Task
  (e.g. a tracker issue with no graders and no default grader policy) are
  the adapter's problem to skip or surface — they never reach the
  scheduler.
* **Outbound** — :meth:`WorkSource.report` receives a :class:`WorkReport`
  every time the orchestrator finishes driving a run for one of the
  source's items. The store remains flywheel's authoritative record; the
  report is a projection of it for the system that owns the work (close a
  ticket, post grader receipts). Reports are delivered best-effort: a
  raising ``report`` is contained by the orchestrator and never unwinds
  the scheduling loop.

:class:`DirectoryWorkSource` is the reference adapter: the historical
``.flywheel/tasks/active/<phase>/*.json`` layout. Its ``report`` is a no-op
because the local store *is* the record (phase archiving stays a separate,
directory-specific flow — see ``archive_completed_phases``).
"""

from __future__ import annotations

import json
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol, runtime_checkable

from flywheel_core.lifecycle import Status
from flywheel_core.loaders import TaskLoadError, load_task_file, task_digest
from flywheel_core.task import Task

# A source's stop-event sink: ``sink(kind, subject, detail)`` appends one
# durable stop record to the orchestrator's append-only ledger without changing
# what the source lists. ``kind`` is a member of
# ``ORCHESTRATOR_STOP_EVENT_KINDS`` (``source-truncation`` /
# ``zero-grader-drop`` for a source), ``subject`` is the source name (its repo
# or tasks_dir path), and ``detail`` names the cause. A source given no sink
# behaves byte-for-byte as before -- the record is an audit witness, never a
# scheduling input.
StopEventSink = Callable[[str, str, str], None]


class WorkSourceError(RuntimeError):
    """Raised when a work source cannot enumerate or compile its items.

    The message always identifies the offending source/item so callers see
    actionable errors. Adapter-internal transport failures (a tracker API
    error, an unreadable directory) surface as this type so the CLI can
    render them uniformly.
    """


@dataclass(frozen=True, kw_only=True)
class WorkItem:
    """One schedulable unit handed to the orchestrator by a work source.

    ``task`` is the compiled, validated core Task — the only thing that
    crosses into flywheel. ``prerequisites`` carries the orchestration-layer
    dependency edges (task ids that must reach DONE first); the core Task
    deliberately has no such field. ``source_ref`` is the adapter's opaque
    handle for the item (a file path, ``owner/repo#123``); it round-trips
    into :class:`WorkReport` and is persisted as the lifecycle's ``source``
    label. ``local_path`` is set only by file-backed sources; consumers
    that derive meaning from an on-disk location (e.g. the worktree
    submitter's phase-from-directory branch naming) read it, and
    non-file sources leave it ``None``.

    ``source_kind``/``source_version``/``source_url`` carry the item's
    provenance for persistence: where it came from (``"directory"``,
    ``"github_issue"``), a stable content hash that changes when the
    operator edits the underlying definition, and a locator (a file path,
    an issue URL). All three are optional so direct ``WorkItem``
    construction without provenance still compiles; adapters populate them.

    ``priority``/``required_capabilities``/``conflict_keys`` are the
    orchestration-layer scheduling metadata (spec 00049). They are not part
    of the core single-task definition, so core ``flywheel`` ignores them;
    file-backed sources read them from the task file's top-level JSON.
    All three default (priority ``0``, both sets empty) so direct
    construction without scheduling metadata still compiles.
    """

    task: Task
    prerequisites: tuple[str, ...] = ()
    source_ref: str
    local_path: Path | None = None
    source_kind: str | None = None
    source_version: str | None = None
    source_url: str | None = None
    priority: int = 0
    required_capabilities: frozenset[str] = frozenset()
    conflict_keys: frozenset[str] = frozenset()


@dataclass(frozen=True, kw_only=True)
class GraderReceipt:
    """One grader execution receipt projected into a :class:`WorkReport`.

    A flattened view of the store's ``GraderResultRecord`` carrying only
    what an external system needs to render "how done was decided":
    position, type, optional name, and the verdict.
    """

    ordinal: int
    grader_type: str
    name: str | None
    passed: bool


@dataclass(frozen=True, kw_only=True)
class WorkReport:
    """Terminal outcome of one orchestrator-driven run, for the work owner.

    ``status`` is the lifecycle status the run ended this drive at —
    ``DONE`` on verified completion, ``FAILED``/``FAILED_VALIDATION`` on
    exhausted budgets, ``INTERRUPTED``/``AWAITING_APPROVAL`` when parked.
    ``graders`` holds the final attempt's receipts (empty when the run
    never reached grading). The evidence is a projection of the store;
    the store row, not this report, is the authoritative record.
    """

    task_id: str
    source_ref: str
    run_id: str
    status: Status
    error: str
    graders: tuple[GraderReceipt, ...]


@runtime_checkable
class WorkSource(Protocol):
    """The orchestrator's boundary with the system that owns the work."""

    def list_work(self) -> Sequence[WorkItem]:
        """Enumerate the current candidate items, compiled and validated.

        Called once per scheduling pass, so implementations should be
        cheap-ish and deterministic in order (ties in selection break by
        enumeration order). Raises :class:`WorkSourceError` (or
        ``flywheel_core.loaders.TaskLoadError``) when the source exists but an
        item cannot compile.
        """
        ...

    def report(self, report: WorkReport) -> None:
        """Receive the outcome of one driven run (best-effort delivery).

        The orchestrator calls this after the consumer ``submit`` step,
        while the task's lease is still held. Exceptions are contained by
        the caller — a failing report costs the notification, never the
        schedule.
        """
        ...


def _read_prerequisites(path: Path) -> tuple[str, ...]:
    """Read a task file's ``prerequisites`` edges (the orchestration DAG).

    Core ``flywheel`` ignores ``prerequisites`` (it is not part of a single
    task's definition), so the orchestrator parses it from the task source
    itself. The file is already known to be valid JSON by the time this is
    called (``load_active_tasks`` loaded the task through it first).
    """
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return ()
    raw = data.get("prerequisites") if isinstance(data, dict) else None
    return tuple(str(p) for p in raw) if isinstance(raw, list) else ()


def _read_str_set(raw: object) -> frozenset[str]:
    """Coerce a JSON list-of-strings into a ``frozenset``; else empty."""
    return (
        frozenset(str(x) for x in raw) if isinstance(raw, list) else frozenset()
    )


def _read_scheduling_metadata(
    path: Path,
) -> tuple[int, frozenset[str], frozenset[str]]:
    """Read a task file's orchestration scheduling metadata (spec 00049).

    ``priority`` (int, default ``0``), ``required_capabilities`` and
    ``conflict_keys`` (string sets, default empty) are orchestration-layer
    keys core ``flywheel`` ignores, so the orchestrator parses them straight
    from the task source, mirroring :func:`_read_prerequisites`. An absent or
    malformed key yields its default. ``bool`` (a JSON ``true``/``false``) is
    rejected as a priority — it is an ``int`` subclass but never a valid
    priority value.
    """
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return (0, frozenset(), frozenset())
    if not isinstance(data, dict):
        return (0, frozenset(), frozenset())
    raw_priority = data.get("priority")
    priority = (
        raw_priority
        if isinstance(raw_priority, int) and not isinstance(raw_priority, bool)
        else 0
    )
    return (
        priority,
        _read_str_set(data.get("required_capabilities")),
        _read_str_set(data.get("conflict_keys")),
    )


def iter_active_phase_dirs(tasks_dir: Path) -> Iterator[Path]:
    """Yield ``active/<phase>`` subdirectories in deterministic order.

    Filename prefix (``NN-...``) sorts the phases. Hidden directories and
    files are skipped.
    """
    active = tasks_dir / "active"
    if not active.is_dir():
        return
    for entry in sorted(active.iterdir()):
        if entry.is_dir() and not entry.name.startswith("."):
            yield entry


def iter_active_task_files(tasks_dir: Path) -> Iterator[Path]:
    """Yield every ``active/<phase>/<task>.json`` in deterministic order."""
    for phase in iter_active_phase_dirs(tasks_dir):
        for entry in sorted(phase.iterdir()):
            if (
                entry.is_file()
                and entry.suffix == ".json"
                and not entry.name.startswith("_")
                and not entry.name.startswith(".")
            ):
                yield entry


def load_active_tasks(tasks_dir: Path) -> list[tuple[Path, Task]]:
    """Load every active task; raise ``TaskLoadError`` on the first bad file.

    The list is in deterministic walk order so ``next`` ties break by
    filename.
    """
    out: list[tuple[Path, Task]] = []
    for path in iter_active_task_files(tasks_dir):
        out.append((path, load_task_file(path)))
    return out


class DirectoryWorkSource:
    """The reference :class:`WorkSource`: a directory of task JSON files.

    Wraps the historical ``<tasks_dir>/active/<phase>/<task>.json`` layout.
    ``source_ref`` is the file path string (exactly what ``run_task_file``
    used to persist as the lifecycle's ``source``), ``local_path`` is the
    same path as a ``Path`` so path-deriving consumers keep working.

    ``report`` is intentionally a no-op: for local files the store is
    already the durable record and phase archiving (with its loop-path
    gate) is a separate directory-shaped flow driven by the worker.

    One unloadable task file (malformed JSON, a definition that fails
    validation) is **skipped, not fatal**: ``list_work`` counts it in
    :attr:`last_skipped_count` and emits one ``log`` line naming the file,
    then returns every other valid item in deterministic walk order. The
    record is what lets a downstream reconciler tell a skip ("one item was
    dropped, investigate it") from an empty source ("there is simply no
    work") -- the two must never look alike. ``log`` (when provided)
    receives one line per skipped file, mirroring the tracker sources.
    """

    #: Provenance ``source_kind`` stamped on every emitted item and on the
    #: ``source_syncs`` row recording a sync over this source.
    source_kind = "directory"

    def __init__(
        self,
        tasks_dir: Path,
        *,
        log: Callable[[str], None] | None = None,
    ) -> None:
        self.tasks_dir = tasks_dir
        self._log = log
        #: Number of task files the most recent :meth:`list_work` skipped
        #: because they could not be loaded. A reconciler reads this to
        #: distinguish "dropped a bad item" from "no work" -- both can
        #: yield a short or empty item list, but only a skip is non-zero.
        self.last_skipped_count = 0

    @property
    def source_name(self) -> str:
        """The source's locus for ``source_syncs`` (D-4): the tasks_dir path."""
        return str(self.tasks_dir)

    def list_work(self) -> list[WorkItem]:
        items: list[WorkItem] = []
        skipped = 0
        for path in iter_active_task_files(self.tasks_dir):
            try:
                task = load_task_file(path)
            except TaskLoadError as exc:
                skipped += 1
                if self._log is not None:
                    self._log(
                        f"[directory] skipping {path}: cannot load task "
                        f"-- {exc}"
                    )
                continue
            priority, required_capabilities, conflict_keys = (
                _read_scheduling_metadata(path)
            )
            items.append(
                WorkItem(
                    task=task,
                    prerequisites=_read_prerequisites(path),
                    source_ref=str(path),
                    local_path=path,
                    source_kind="directory",
                    source_version=task_digest(task),
                    source_url=str(path),
                    priority=priority,
                    required_capabilities=required_capabilities,
                    conflict_keys=conflict_keys,
                )
            )
        self.last_skipped_count = skipped
        return items

    def report(self, report: WorkReport) -> None:
        return None


__all__ = [
    "DirectoryWorkSource",
    "GraderReceipt",
    "StopEventSink",
    "WorkItem",
    "WorkReport",
    "WorkSource",
    "WorkSourceError",
    "iter_active_phase_dirs",
    "iter_active_task_files",
    "load_active_tasks",
]

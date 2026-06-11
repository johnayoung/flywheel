"""File implementation of the ``TelemetrySink`` Protocol from
``flywheel_core.store_protocols``.

Writes one append-only JSONL file per run at
``<logs_root>/runs/<run_id>.jsonl`` (default logs root
``.flywheel/logs``): one flushed JSON object per line, in emission
order. File write order is the canonical observability ordering for the
run; the sink never rewrites, reorders, or truncates a file it has
appended to.

Durability semantics: each append is a single buffered write followed by
a flush, so a line reaches the OS as soon as ``append_telemetry``
returns. No fsync — telemetry loss on power failure is acceptable per
the data taxonomy; the relational ledger remains the source of truth
for state.

Concurrency: distinct runs write disjoint files keyed by ``run_id``, so
concurrent runs never contend. Creating the ``runs/`` directory is
idempotent (``exist_ok``) and safe when multiple sinks race on first
append. Within one sink instance a lock serializes appends so
interleaved callers cannot split a line.

Like the run database, run files are verbatim and sensitive by default;
redaction is read-time (see ``flywheel_core.redaction``).
"""

from __future__ import annotations

import json
import threading
from pathlib import Path
from typing import TextIO

from flywheel_core.store_protocols import TelemetryRecord


DEFAULT_LOGS_ROOT = Path(".flywheel/logs")

_RUNS_DIRNAME = "runs"


class FileTelemetrySink:
    """JSONL-file :class:`flywheel_core.store_protocols.TelemetrySink`.

    One instance serves any number of runs: appends are routed to
    ``<logs_root>/runs/<record.run_id>.jsonl`` and the file handle is
    cached per run for the lifetime of the sink (or until
    :meth:`close`). Files are opened in append mode, so a second sink —
    or a sink re-created after :meth:`close` — continues the same file
    without rewriting prior lines.

    Usable as a context manager; ``close()`` flushes and releases every
    cached handle and is idempotent.
    """

    def __init__(self, logs_root: str | Path = DEFAULT_LOGS_ROOT) -> None:
        self._runs_dir = Path(logs_root) / _RUNS_DIRNAME
        self._handles: dict[str, TextIO] = {}
        self._lock = threading.Lock()

    @property
    def runs_dir(self) -> Path:
        """Directory holding the per-run JSONL files."""
        return self._runs_dir

    def path_for(self, run_id: str) -> Path:
        """Return the JSONL path for ``run_id`` without creating it."""
        _validate_run_id(run_id)
        return self._runs_dir / f"{run_id}.jsonl"

    def append_telemetry(self, record: TelemetryRecord) -> None:
        """Append ``record`` as one JSON line to the run's file.

        The line is flushed before returning; appends are strictly
        ordered per run within this sink instance.
        """
        line = json.dumps(_record_to_json(record), ensure_ascii=False)
        with self._lock:
            handle = self._handle_for(record.run_id)
            handle.write(line + "\n")
            handle.flush()

    def close(self) -> None:
        """Flush and release every cached file handle. Idempotent."""
        with self._lock:
            handles = list(self._handles.values())
            self._handles.clear()
        for handle in handles:
            handle.close()

    def __enter__(self) -> FileTelemetrySink:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()

    # --- internals -----------------------------------------------------

    def _handle_for(self, run_id: str) -> TextIO:
        """Return the cached append handle for ``run_id``, opening it
        (and creating ``runs/`` idempotently) on first use.

        Caller holds ``self._lock``.
        """
        handle = self._handles.get(run_id)
        if handle is None:
            path = self.path_for(run_id)
            self._runs_dir.mkdir(parents=True, exist_ok=True)
            handle = path.open("a", encoding="utf-8")
            self._handles[run_id] = handle
        return handle


def _validate_run_id(run_id: str) -> None:
    """Reject run ids that cannot form a safe single-segment filename."""
    if not run_id or "/" in run_id or "\\" in run_id or run_id in {".", ".."}:
        raise ValueError(f"run_id {run_id!r} is not a valid file key")


def _record_to_json(record: TelemetryRecord) -> dict[str, object]:
    """Project a :class:`TelemetryRecord` onto its JSONL line shape.

    Datetimes become ISO-8601 strings; the payload mapping is persisted
    verbatim. ``run_id`` is included so each line is self-contained even
    when the file is moved or concatenated by external tooling.
    """
    return {
        "kind": record.kind,
        "ts": record.ts.isoformat(),
        "run_id": record.run_id,
        "attempt_number": record.attempt_number,
        "iteration_number": record.iteration_number,
        "payload": dict(record.payload),
    }


__all__ = ["DEFAULT_LOGS_ROOT", "FileTelemetrySink"]

"""Byte-offset tailer over a run's telemetry JSONL file.

The run file at ``<logs_root>/runs/<run_id>.jsonl`` (written by
:class:`flywheel_core.telemetry_file.FileTelemetrySink`) replaced the
store as the durable telemetry destination (spec 00025). This module is
the single read primitive every observability consumer shares: it turns
the file's lines back into the :class:`AuditRecord` shapes
(:class:`EventRecord` / :class:`SdkMessageRecord`) the readers, the
:class:`~flywheel_core.redaction.Redactor`, and the TUI classifier
already speak, so the read path's record contract survives the
destination change.

Cursor semantics (spec FR-8):

* The cursor is a ``(byte offset, line count)`` pair. The offset is the
  first unconsumed byte; the line count is the number of complete lines
  consumed so far and doubles as the reconstructed records' monotonic
  ``sequence`` (file write order is the canonical observability
  ordering for the run).
* A missing file reads as empty — follow-mode callers simply poll until
  it appears.
* A partial trailing line (a crash mid-write, or a read racing the
  sink's flush) is withheld: the cursor does not advance past it, so
  the next read re-attempts it once the writer completes the line. With
  ``eof_final=True`` (the reader's last pass after the lifecycle went
  terminal) the partial line is discarded instead.

Line classification mirrors the old merged stream's surface: SDK
message lines become :class:`SdkMessageRecord`, ``harness.*`` telemetry
lines become :class:`EventRecord`, and ``domain.*`` ledger mirrors are
skipped — ledger state was never part of the audit stream's record
surface; consumers read it from the store. A malformed (but complete)
line is skipped rather than raised so one corrupt write cannot wedge a
live tail.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from flywheel_core.store_protocols import (
    AuditRecord,
    EventRecord,
    SdkMessageRecord,
)


_DOMAIN_MIRROR_PREFIX = "domain."
_TELEMETRY_EVENT_PREFIX = "harness."

_EPOCH = datetime.fromtimestamp(0, tz=timezone.utc)


@dataclass(frozen=True)
class FileCursor:
    """Read position inside a run's telemetry JSONL file.

    ``offset`` is the byte position of the first unconsumed byte;
    ``line`` is the count of complete lines consumed so far (the
    sequence source for reconstructed records). Immutable so callers
    can hold the previous cursor for retry / comparison.
    """

    offset: int = 0
    line: int = 0


def run_file_path(logs_root: Path, run_id: str) -> Path:
    """Return the JSONL path for ``run_id`` under ``logs_root``.

    Mirrors :meth:`flywheel_core.telemetry_file.FileTelemetrySink.path_for`
    so writer and readers agree on the layout without sharing an
    instance.
    """
    return Path(logs_root) / "runs" / f"{run_id}.jsonl"


def read_records_since(
    path: Path,
    cursor: FileCursor,
    *,
    eof_final: bool = False,
) -> tuple[list[AuditRecord], FileCursor]:
    """Read every complete line past ``cursor`` and reconstruct records.

    Returns the parsed records (skipped lines still advance the cursor)
    and the new cursor. A missing file returns ``([], cursor)``. See
    the module docstring for partial-line and classification rules.
    """
    try:
        with open(path, "rb") as handle:
            handle.seek(cursor.offset)
            data = handle.read()
    except FileNotFoundError:
        return [], cursor
    if not data:
        return [], cursor

    records: list[AuditRecord] = []
    offset = cursor.offset
    line_no = cursor.line
    start = 0
    while True:
        newline = data.find(b"\n", start)
        if newline == -1:
            break
        raw = data[start:newline]
        start = newline + 1
        offset = cursor.offset + start
        line_no += 1
        record = _record_from_line(raw, sequence=line_no)
        if record is not None:
            records.append(record)
    if eof_final and start < len(data):
        # Crash mid-write: the trailing bytes will never be completed.
        # Discard them so a re-read of a finished run is idempotent.
        offset = cursor.offset + len(data)
    return records, FileCursor(offset=offset, line=line_no)


def _record_from_line(raw: bytes, *, sequence: int) -> AuditRecord | None:
    """Reconstruct one :class:`AuditRecord` from a JSONL line.

    Returns ``None`` for domain mirrors and for malformed lines (both
    advance the cursor at the call site; neither reaches consumers).
    """
    try:
        obj = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return None
    if not isinstance(obj, dict):
        return None
    kind = obj.get("kind")
    if not isinstance(kind, str) or not kind:
        return None
    if kind.startswith(_DOMAIN_MIRROR_PREFIX):
        return None
    run_id = obj.get("run_id")
    payload = obj.get("payload")
    attempt_number = obj.get("attempt_number")
    iteration_number = obj.get("iteration_number")
    record_payload: dict[str, Any] = (
        dict(payload) if isinstance(payload, dict) else {}
    )
    ts = _parse_ts(obj.get("ts"))
    if kind.startswith(_TELEMETRY_EVENT_PREFIX):
        return EventRecord(
            run_id=str(run_id) if isinstance(run_id, str) else "",
            ts=ts,
            kind=kind,
            payload=record_payload,
            attempt_number=(
                attempt_number
                if isinstance(attempt_number, int)
                and not isinstance(attempt_number, bool)
                else None
            ),
            sequence=sequence,
        )
    return SdkMessageRecord(
        run_id=str(run_id) if isinstance(run_id, str) else "",
        attempt_number=(
            attempt_number
            if isinstance(attempt_number, int)
            and not isinstance(attempt_number, bool)
            else 0
        ),
        iteration_number=(
            iteration_number
            if isinstance(iteration_number, int)
            and not isinstance(iteration_number, bool)
            else 0
        ),
        message_type=kind,
        payload=record_payload,
        ts=ts,
        sequence=sequence,
    )


def _parse_ts(value: object) -> datetime:
    """Parse the line's ISO-8601 timestamp, defensively.

    A missing or malformed ``ts`` falls back to the epoch rather than
    raising — the timestamp is presentation metadata; ordering comes
    from the file position.
    """
    if isinstance(value, str):
        try:
            return datetime.fromisoformat(value)
        except ValueError:
            return _EPOCH
    return _EPOCH


__all__ = ["FileCursor", "read_records_since", "run_file_path"]

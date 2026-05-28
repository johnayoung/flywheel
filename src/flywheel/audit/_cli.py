"""Command-line surface for ``python -m flywheel.audit``.

Thin wrapper over :func:`flywheel.audit.stream` that handles argument
parsing, store construction, and two output formats:

* **Default** -- one record per line, formatted as::

      <iso-ts> seq=<n> attempt=<a> iter=<i> kind=<k> | <preview>

  where ``<preview>`` is a short single-line summary capped at
  :data:`PREVIEW_MAX_CHARS` characters. Longer payloads are truncated
  and a hint pointing to ``--json`` is appended so the reader can fetch
  the full payload on demand.

* **NDJSON** (``--json``) -- one JSON object per line; every record
  round-trips through :func:`json.loads` losslessly. Non-JSON-native
  values (``datetime``, ``bytes``) are coerced via :func:`_json_default`.

The CLI is intentionally minimal -- the spec defers ``--kind``,
``--attempt``, ``--summary`` flags to a follow-up feature.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import sys
from collections.abc import Iterable, Mapping
from datetime import datetime
from pathlib import Path
from typing import Any, TextIO

from flywheel.audit import stream
from flywheel.store_protocols import (
    AuditRecord,
    EventRecord,
    SdkMessageRecord,
)
from flywheel.store_sqlite import SqliteStore


# Default store path used when neither ``--db`` nor ``$FLYWHEEL_DB`` is
# set. Matches the spec's "./flywheel.db" fallback rather than
# workflow.py's ".workflow/flywheel.sqlite" -- the audit CLI is a
# debugging tool that may run outside a ``.workflow/`` layout.
DEFAULT_DB_PATH: Path = Path("flywheel.db")
DEFAULT_POLL_INTERVAL: float = 0.25

# Total preview budget for the default human-readable mode. Fixed (not
# terminal-width-derived) so output is reproducible across pipes and
# tests; the spec calls out the ~80-char tail explicitly.
PREVIEW_MAX_CHARS: int = 80

# Truncation hint appended when the preview body would exceed
# :data:`PREVIEW_MAX_CHARS`. The hint occupies part of the budget so
# the final line never exceeds it.
TRUNCATION_HINT: str = "... (use --json for full payload)"


def _resolve_db(arg: str | None) -> Path:
    """Resolve the ``--db`` argument with ``$FLYWHEEL_DB`` fallback.

    Precedence: explicit ``--db`` > ``$FLYWHEEL_DB`` env var >
    :data:`DEFAULT_DB_PATH`. The empty-string env var is treated as
    unset so a stray ``FLYWHEEL_DB=`` does not silently shadow the
    default.
    """
    if arg:
        return Path(arg)
    env = os.environ.get("FLYWHEEL_DB")
    if env:
        return Path(env)
    return DEFAULT_DB_PATH


def _json_default(value: Any) -> Any:
    """Fallback encoder for objects ``json.dumps`` cannot handle.

    The library already persists payloads as JSON-compatible dicts so
    this is rarely hit; the spec calls for defensive coverage of
    ``datetime`` (ISO-8601) and ``bytes`` (base64) shapes. Any other
    non-native type raises :class:`TypeError` so we never silently emit
    truncated or stringified data.
    """
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, (bytes, bytearray)):
        return base64.b64encode(bytes(value)).decode("ascii")
    raise TypeError(f"cannot JSON-serialize {type(value).__name__}")


def _record_kind(record: AuditRecord) -> str:
    """Return the ``kind=<k>`` token for the default-mode line.

    Mirrors the convention used by the logger emitter
    (``event:<kind>`` / ``sdk:<message_type>``) so default-mode output
    reads the same way as :func:`flywheel.audit.attach_logger` log
    messages.
    """
    if isinstance(record, EventRecord):
        return f"event:{record.kind}"
    return f"sdk:{record.message_type}"


def _record_attempt(record: AuditRecord) -> str:
    """Stringify ``attempt_number`` with ``"-"`` for ``None``.

    ``EventRecord.attempt_number`` is optional; we render the empty
    case as ``"-"`` so default-mode columns line up regardless of
    record type.
    """
    if isinstance(record, EventRecord):
        if record.attempt_number is None:
            return "-"
        return str(record.attempt_number)
    return str(record.attempt_number)


def _record_iter(record: AuditRecord) -> str:
    """Stringify ``iteration_number`` with ``"-"`` for events.

    Events have no iteration index by construction (a single attempt
    spans many iterations); rendering them as ``"-"`` keeps the
    column-aligned look without lying about which records carry an
    iteration.
    """
    if isinstance(record, SdkMessageRecord):
        return str(record.iteration_number)
    return "-"


def _record_iter_value(record: AuditRecord) -> int | None:
    """Return the JSON-mode ``iteration_number`` (``None`` for events)."""
    if isinstance(record, SdkMessageRecord):
        return record.iteration_number
    return None


def _payload_preview(record: AuditRecord) -> str:
    """Return a single-line truncated payload preview.

    The payload is rendered as compact JSON (sorted keys for
    determinism), embedded newlines flattened to spaces. If the result
    is longer than :data:`PREVIEW_MAX_CHARS` we replace the tail with
    :data:`TRUNCATION_HINT` so the total stays within budget.
    """
    # Both record arms carry a ``Mapping[str, Any]`` ``payload`` so the
    # rendering is identical regardless of type; the per-type discriminator
    # already lives in ``kind=<k>``.
    payload: Mapping[str, Any] = record.payload
    body = json.dumps(
        dict(payload), default=_json_default, sort_keys=True, separators=(",", ":")
    )
    body = body.replace("\n", " ").replace("\r", " ")
    if len(body) <= PREVIEW_MAX_CHARS:
        return body
    keep = PREVIEW_MAX_CHARS - len(TRUNCATION_HINT)
    if keep < 0:
        keep = 0
    return body[:keep] + TRUNCATION_HINT


def _render_default(record: AuditRecord) -> str:
    """Format one record as a default-mode line.

    Format: ``<iso-ts> seq=<n> attempt=<a> iter=<i> kind=<k> | <preview>``.
    Sequence is rendered as ``"?"`` if absent (the store contract says
    it never will be, but we don't crash on the defensive case).
    """
    ts = record.ts.isoformat()
    seq = "?" if record.sequence is None else str(record.sequence)
    attempt = _record_attempt(record)
    iter_str = _record_iter(record)
    kind = _record_kind(record)
    preview = _payload_preview(record)
    return f"{ts} seq={seq} attempt={attempt} iter={iter_str} kind={kind} | {preview}"


def _render_json(record: AuditRecord) -> str:
    """Format one record as an NDJSON object.

    The schema is fixed so downstream consumers can rely on the field
    set without sniffing the record type: ``iteration_number`` is
    ``null`` for events, ``kind_or_message_type`` carries the per-type
    discriminator without a prefix (callers can keep their own
    isinstance-style switch).
    """
    obj: dict[str, Any] = {
        "ts": record.ts.isoformat(),
        "sequence": record.sequence,
        "run_id": record.run_id,
        "attempt_number": record.attempt_number,
        "iteration_number": _record_iter_value(record),
        "kind_or_message_type": (
            record.kind
            if isinstance(record, EventRecord)
            else record.message_type
        ),
        "payload": dict(record.payload),
    }
    return json.dumps(obj, default=_json_default, sort_keys=True)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m flywheel.audit",
        description=(
            "Print the totally-ordered audit stream for one run. Default "
            "output is a human-readable one-line-per-record view; --json "
            "switches to NDJSON; --follow tails until the lifecycle "
            "reaches a terminal status (Ctrl-C exits cleanly with code 130)."
        ),
    )
    parser.add_argument(
        "run_id",
        help="The run_id whose audit stream to read.",
    )
    parser.add_argument(
        "--db",
        default=None,
        help=(
            f"SQLite database path (default: $FLYWHEEL_DB or "
            f"{DEFAULT_DB_PATH})."
        ),
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit one JSON object per record (NDJSON) instead of the text view.",
    )
    parser.add_argument(
        "--follow",
        action="store_true",
        help=(
            "Tail the audit stream live; exits when the lifecycle "
            "reaches a terminal status (DONE, FAILED, INTERNAL_ERROR, "
            "INTERRUPTED)."
        ),
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=DEFAULT_POLL_INTERVAL,
        help=(
            f"Polling interval for --follow in seconds "
            f"(default: {DEFAULT_POLL_INTERVAL})."
        ),
    )
    return parser


def main(
    argv: Iterable[str] | None = None,
    *,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    """Entry point for ``python -m flywheel.audit``.

    Returns the process exit code. ``stdout``/``stderr`` are injectable
    so unit tests can capture output without subprocess overhead; both
    default to the real streams.
    """
    parser = _build_parser()
    args = parser.parse_args(list(argv) if argv is not None else None)
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr

    db_path = _resolve_db(args.db)
    store = SqliteStore(db_path)
    try:
        # Spec error-handling table: unknown run_id yields nothing and
        # exits 0 with a clear stderr message. We check the lifecycle
        # *before* draining so the message fires whether or not any
        # records exist (an orphaned event without a lifecycle row is a
        # store-integrity bug, not a user-facing error).
        if store.load_lifecycle(args.run_id) is None:
            print(f"no records for run_id {args.run_id}", file=err)
            return 0

        iterator = stream(
            args.run_id,
            store=store,
            follow=args.follow,
            poll_interval=args.poll_interval,
        )
        render = _render_json if args.json else _render_default
        try:
            for record in iterator:
                print(render(record), file=out)
                out.flush()
        except KeyboardInterrupt:
            # SIGINT under --follow: exit cleanly per the spec without
            # a Python stack trace. Code 130 matches the conventional
            # "terminated by Ctrl-C" exit; documented in --help.
            return 130
    finally:
        store.close()
    return 0


__all__ = [
    "DEFAULT_DB_PATH",
    "DEFAULT_POLL_INTERVAL",
    "PREVIEW_MAX_CHARS",
    "TRUNCATION_HINT",
    "main",
]

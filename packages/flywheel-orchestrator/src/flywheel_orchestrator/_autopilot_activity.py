"""Cross-process activity snapshot for the autopilot daemon.

The autopilot daemon (:mod:`flywheel_orchestrator._autopilot_run`) runs detached
from the console that spawned it (``start_new_session=True``), so the only
channel back to the console's status surface is a file. Each cycle the daemon
writes an :class:`AutopilotActivity` snapshot here; the console-side
``AutopilotSupervisor`` reads it best-effort (mirroring
``read_supervised_death_reason``) to show what the daemon is doing *right now* --
which cycle it is on, the last cycle's emitted/dropped counts, and when the next
cycle fires.

The snapshot doubles as the daemon's cross-process **liveness record**: it
carries the daemon ``pid`` plus an ``expires_at`` freshness deadline (epoch
seconds) that the daemon pushes forward on every write. A second supervisor
reads that deadline through :func:`read_live_activity` to decide whether a live
daemon is already running -- if so it adopts it (reports ``DETACHED``) instead
of spawning a duplicate. The staleness rule mirrors the worker lease exactly
(``flywheel._worker_supervisor.has_live_lease``): a record whose ``expires_at``
is at/before ``now`` reads as not-live, just like a lapsed ``lease_expires_at``,
so a truly dead daemon is respawned rather than adopted forever.

This file is a live activity surface only; it is NOT authoritative lifecycle
state. A stale file left by a previous daemon is ignored on the read side by
pid mismatch, and any malformed/missing/old-schema file reads back as ``None``.
"""

from __future__ import annotations

import json
import os
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

# Bump when the on-disk shape changes incompatibly; a reader that sees an
# unrecognized version treats the file as absent rather than guessing.
ACTIVITY_SCHEMA_VERSION = 1

# The phases the daemon reports. Coarse by design: the console shows per-cycle
# activity, not intra-pass steps (a single ``run_refill_pass`` is one opaque
# async call). ``starting`` is the brief window before the first cycle records.
PHASE_STARTING = "starting"
PHASE_RUNNING = "running"
PHASE_IDLE = "idle"


@dataclass(frozen=True, kw_only=True)
class EmittedSummary:
    """One emitted task's id and tier, for the status line's breakdown."""

    task_id: str
    tier: int


@dataclass(frozen=True, kw_only=True)
class AutopilotActivity:
    """What the autopilot daemon is doing right now, for the status surface.

    Timestamps are epoch seconds (``time.time()``) so the reader can compute a
    countdown without parsing -- both processes share one machine clock.
    ``next_cycle_at`` is set only while ``phase == PHASE_IDLE`` (sleeping between
    cycles); during a cycle it is ``None`` (the daemon is busy, not waiting).

    ``expires_at`` is the liveness-record freshness deadline (epoch seconds):
    the wall-clock instant at/after which this record reads as not-live. The
    daemon pushes it forward on every write to ``now + interval + grace`` so a
    healthy daemon -- idling between cycles or busy inside one -- always reads
    live, while a dead daemon's last record lapses and reads not-live. It is the
    autopilot analogue of the worker lease's ``lease_expires_at``; the read is
    the identical ``deadline > now`` comparison (see :func:`is_record_live`). A
    record with ``expires_at is None`` (e.g. an older writer) reads not-live.
    """

    pid: int
    phase: str
    cycle_index: int
    updated_at: float
    interval_seconds: float
    next_cycle_at: float | None = None
    expires_at: float | None = None
    last_emitted: int = 0
    last_dropped: int = 0
    last_reason: str = ""
    last_relevant_tiers: tuple[int, ...] = ()
    last_emitted_tasks: tuple[EmittedSummary, ...] = ()


def _to_dict(activity: AutopilotActivity) -> dict[str, object]:
    return {
        "schema_version": ACTIVITY_SCHEMA_VERSION,
        "pid": activity.pid,
        "phase": activity.phase,
        "cycle_index": activity.cycle_index,
        "updated_at": activity.updated_at,
        "interval_seconds": activity.interval_seconds,
        "next_cycle_at": activity.next_cycle_at,
        "expires_at": activity.expires_at,
        "last_emitted": activity.last_emitted,
        "last_dropped": activity.last_dropped,
        "last_reason": activity.last_reason,
        "last_relevant_tiers": list(activity.last_relevant_tiers),
        "last_emitted_tasks": [
            {"task_id": e.task_id, "tier": e.tier}
            for e in activity.last_emitted_tasks
        ],
    }


def _from_dict(data: object) -> AutopilotActivity | None:
    if not isinstance(data, dict):
        return None
    if data.get("schema_version") != ACTIVITY_SCHEMA_VERSION:
        return None
    try:
        pid = int(data["pid"])
        phase = str(data["phase"])
        cycle_index = int(data["cycle_index"])
        updated_at = float(data["updated_at"])
        interval_seconds = float(data["interval_seconds"])
    except (KeyError, TypeError, ValueError):
        return None

    raw_next = data.get("next_cycle_at")
    next_cycle_at: float | None
    try:
        next_cycle_at = None if raw_next is None else float(raw_next)
    except (TypeError, ValueError):
        next_cycle_at = None

    raw_expires = data.get("expires_at")
    expires_at: float | None
    try:
        expires_at = None if raw_expires is None else float(raw_expires)
    except (TypeError, ValueError):
        expires_at = None

    tasks: list[EmittedSummary] = []
    raw_tasks = data.get("last_emitted_tasks")
    if isinstance(raw_tasks, list):
        for entry in raw_tasks:
            if not isinstance(entry, dict):
                continue
            try:
                tasks.append(
                    EmittedSummary(
                        task_id=str(entry["task_id"]), tier=int(entry["tier"])
                    )
                )
            except (KeyError, TypeError, ValueError):
                continue

    raw_tiers = data.get("last_relevant_tiers")
    tiers: tuple[int, ...] = ()
    if isinstance(raw_tiers, list):
        parsed: list[int] = []
        for t in raw_tiers:
            try:
                parsed.append(int(t))
            except (TypeError, ValueError):
                continue
        tiers = tuple(parsed)

    return AutopilotActivity(
        pid=pid,
        phase=phase,
        cycle_index=cycle_index,
        updated_at=updated_at,
        interval_seconds=interval_seconds,
        next_cycle_at=next_cycle_at,
        expires_at=expires_at,
        last_emitted=int(data.get("last_emitted", 0) or 0),
        last_dropped=int(data.get("last_dropped", 0) or 0),
        last_reason=str(data.get("last_reason", "")),
        last_relevant_tiers=tiers,
        last_emitted_tasks=tuple(tasks),
    )


def write_activity(path: Path, activity: AutopilotActivity) -> None:
    """Atomically write the activity snapshot to ``path``.

    Writes a sibling temp file and ``os.replace``s it over the target so a
    reader never observes a half-written JSON document. The parent directory is
    created if missing. Best-effort by contract on the daemon side: the caller
    should not let an activity-write failure crash a refill cycle.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(
        dir=str(path.parent), prefix=".activity-", suffix=".tmp"
    )
    tmp = Path(tmp_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(_to_dict(activity), handle)
        os.replace(tmp, path)
    except OSError:
        tmp.unlink(missing_ok=True)
        raise


def read_activity(path: Path | None) -> AutopilotActivity | None:
    """Read the activity snapshot, or ``None`` if absent/malformed.

    Never raises: a missing file, an unreadable file, malformed JSON, or an
    unrecognized schema version all read back as ``None`` so the status surface
    degrades to plain liveness rather than erroring.
    """
    if path is None:
        return None
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None
    try:
        data = json.loads(text)
    except (ValueError, TypeError):
        return None
    return _from_dict(data)


def is_record_live(activity: AutopilotActivity | None, *, now: float) -> bool:
    """Whether the record represents a live daemon at ``now`` (epoch seconds).

    The autopilot analogue of ``has_live_lease``'s ``lease_expires_at > now``:
    a record whose ``expires_at`` freshness deadline is strictly in the future
    reads as live; a record at/before ``now`` (a lapsed record) reads as
    not-live, so a truly dead daemon is respawned rather than adopted forever.
    A missing record (``None``) or one without a freshness deadline
    (``expires_at is None``) is not-live -- the same "no lease, no live worker"
    default the worker takes for an absent ``task_claims`` row.
    """
    if activity is None or activity.expires_at is None:
        return False
    return activity.expires_at > now


def read_live_activity(
    path: Path | None, *, now: float | None = None
) -> AutopilotActivity | None:
    """Read the liveness record and return it only if it reads live at ``now``.

    Composes :func:`read_activity` with :func:`is_record_live` so a caller
    (a second supervisor) gets the live daemon's record -- ``pid`` and all --
    to adopt, or ``None`` when there is no live daemon to adopt (missing,
    malformed, or stale record). ``now`` defaults to the shared machine clock
    (``time.time()``); tests inject a fixed instant. The returned record's
    ``pid`` lets the caller distinguish a foreign daemon from a child it owns
    (the same pid guard the SUPERVISED activity read uses).
    """
    activity = read_activity(path)
    moment = now if now is not None else time.time()
    return activity if is_record_live(activity, now=moment) else None


__all__ = [
    "ACTIVITY_SCHEMA_VERSION",
    "PHASE_IDLE",
    "PHASE_RUNNING",
    "PHASE_STARTING",
    "AutopilotActivity",
    "EmittedSummary",
    "is_record_live",
    "read_activity",
    "read_live_activity",
    "write_activity",
]

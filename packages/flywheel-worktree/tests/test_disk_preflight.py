"""Disk/inode preflight guards the authoritative ledger write.

Below the configured free-bytes/free-inodes threshold the worker records a
queryable degraded-space witness and DECLINES the authoritative store write
(``append_domain_event``) before it can be attempted -- so an ENOSPC crash can
never tear a half-written store row. Above threshold the same write is reached
untouched. The probe lives entirely in the worker layer
(``flywheel_worktree._disk_preflight``); ``flywheel_core.task`` / ``.lifecycle``
stay pure.

The two graded cases assert the wired behavior through the real
``GitWorktreeSubmitter._record_landing_park``; the rest pin the classifier
edges (either dimension trips), the direct guard contract, fail-open probing,
and the telemetry mirror.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from typing import Callable

from flywheel_core.events import DomainEvent, LandingParked
from flywheel_core.store_protocols import DegradedSpaceRecord, TelemetryRecord
from flywheel_worktree import worker
from flywheel_worktree._disk_preflight import (
    DEFAULT_MIN_FREE_BYTES,
    DEFAULT_MIN_FREE_INODES,
    DEGRADED_SPACE_KIND,
    DiskPreflight,
    DiskThreshold,
    SpaceProbe,
    classify,
    probe_path,
)

# Comfortably-above-threshold constants used by the "healthy" probes.
AMPLE_BYTES = 10 * 1024 * 1024 * 1024  # 10 GiB
AMPLE_INODES = 10_000_000


def _fixed_probe(
    *, free_bytes: int, free_inodes: int
) -> Callable[[Path | str], SpaceProbe]:
    """A probe that ignores the path and reports fixed free capacity, so a
    test can simulate a near-full disk without actually filling one."""

    def probe(_path: Path | str) -> SpaceProbe:
        return SpaceProbe(free_bytes=free_bytes, free_inodes=free_inodes)

    return probe


class _RecordingLedger:
    """Minimal ``LandingLedger``: records every store touch so a test can
    prove the authoritative write was (or was not) reached."""

    def __init__(self) -> None:
        self.events: list[DomainEvent] = []
        self.load_calls = 0

    class _Lifecycle:
        version = 0

    def load_lifecycle(self, run_id: str) -> "_RecordingLedger._Lifecycle":
        self.load_calls += 1
        return self._Lifecycle()

    def append_domain_event(
        self, event: DomainEvent, *, expected_version: int
    ) -> "_RecordingLedger._Lifecycle":
        self.events.append(event)
        return self._Lifecycle()


def _submitter(
    tmp_path: Path,
    *,
    store: _RecordingLedger,
    preflight: DiskPreflight,
) -> worker.GitWorktreeSubmitter:
    """A submitter wired to a recording ledger and an injected preflight.

    ``_record_landing_park`` touches only ``store``, ``disk_preflight``,
    ``repo_root`` and ``log`` -- no git -- so bare temp paths suffice.
    """
    return worker.GitWorktreeSubmitter(
        repo_root=tmp_path,
        tasks_dir=tmp_path / "tasks",
        worktrees_dir=tmp_path / "worktrees",
        phase_base="main",
        lock_path=tmp_path / ".merge.lock",
        log=lambda _m: None,
        store=store,  # type: ignore[arg-type]
        disk_preflight=preflight,
    )


# --- graded cases -----------------------------------------------------------


def test_below_threshold_degrades_and_skips_store(tmp_path: Path) -> None:
    """Below threshold: a queryable degraded-space record appears AND the
    authoritative ledger write is never invoked (not even ``load_lifecycle``),
    so no half-written row is possible."""
    ledger = _RecordingLedger()
    preflight = DiskPreflight(
        probe=_fixed_probe(free_bytes=1 * 1024 * 1024, free_inodes=AMPLE_INODES),
    )
    submitter = _submitter(tmp_path, store=ledger, preflight=preflight)

    submitter._record_landing_park(
        "run-below", park_kind="divergent-base", detail="base diverged"
    )

    # The crashing write was declined BEFORE it was attempted.
    assert ledger.load_calls == 0
    assert ledger.events == []

    # ... and the decline is queryable on the worker's status surface.
    assert len(submitter.disk_preflight.records) == 1
    record = submitter.disk_preflight.records[0]
    assert isinstance(record, DegradedSpaceRecord)
    assert record.below_bytes is True
    assert record.below_inodes is False
    assert record.run_id == "run-below"
    assert record.path == str(tmp_path)


def test_above_threshold_proceeds(tmp_path: Path) -> None:
    """Above threshold: the authoritative ledger write IS reached and no
    degraded-space record is produced (the preflight must not block healthy
    operation)."""
    ledger = _RecordingLedger()
    preflight = DiskPreflight(
        probe=_fixed_probe(free_bytes=AMPLE_BYTES, free_inodes=AMPLE_INODES),
    )
    submitter = _submitter(tmp_path, store=ledger, preflight=preflight)

    submitter._record_landing_park(
        "run-above", park_kind="divergent-base", detail="base diverged"
    )

    # The store write was reached normally.
    assert ledger.load_calls == 1
    parked = [e for e in ledger.events if isinstance(e, LandingParked)]
    assert len(parked) == 1
    assert parked[0].run_id == "run-above"
    assert parked[0].park_kind == "divergent-base"

    # No false degrade.
    assert submitter.disk_preflight.records == []


# --- classifier edges: either dimension trips -------------------------------


def test_inode_exhaustion_with_bytes_free_degrades(tmp_path: Path) -> None:
    """Inodes below threshold while bytes are ample still declines the write."""
    ledger = _RecordingLedger()
    preflight = DiskPreflight(
        probe=_fixed_probe(free_bytes=AMPLE_BYTES, free_inodes=8),
    )
    submitter = _submitter(tmp_path, store=ledger, preflight=preflight)

    submitter._record_landing_park(
        "run-inode", park_kind="divergent-base", detail="base diverged"
    )

    assert ledger.load_calls == 0
    assert ledger.events == []
    assert len(submitter.disk_preflight.records) == 1
    record = submitter.disk_preflight.records[0]
    assert record.below_inodes is True
    assert record.below_bytes is False


def test_bytes_exhaustion_with_inodes_free_degrades() -> None:
    """Bytes below threshold while inodes are ample trips the byte dimension."""
    record = classify(
        SpaceProbe(free_bytes=16, free_inodes=AMPLE_INODES),
        DiskThreshold(),
        path="/data",
        now=datetime.now(timezone.utc),
    )
    assert record is not None
    assert record.below_bytes is True
    assert record.below_inodes is False


def test_healthy_probe_classifies_as_none() -> None:
    """Both dimensions above threshold classifies as healthy (``None``)."""
    assert (
        classify(
            SpaceProbe(free_bytes=AMPLE_BYTES, free_inodes=AMPLE_INODES),
            DiskThreshold(),
            path="/data",
            now=datetime.now(timezone.utc),
        )
        is None
    )


# --- direct guard contract --------------------------------------------------


def test_guard_skips_write_below_threshold() -> None:
    calls: list[str] = []
    preflight = DiskPreflight(
        probe=_fixed_probe(free_bytes=1, free_inodes=AMPLE_INODES),
    )

    result = preflight.guard("/x", lambda: calls.append("wrote") or "ok")

    assert result is None
    assert calls == []
    assert len(preflight.records) == 1


def test_guard_reaches_write_above_threshold() -> None:
    calls: list[str] = []
    preflight = DiskPreflight(
        probe=_fixed_probe(free_bytes=AMPLE_BYTES, free_inodes=AMPLE_INODES),
    )

    def _write() -> str:
        calls.append("wrote")
        return "ok"

    result = preflight.guard("/x", _write)

    assert result == "ok"
    assert calls == ["wrote"]
    assert preflight.records == []


def test_probe_failure_is_fail_open() -> None:
    """An unmeasurable disk must NOT be mistaken for a full one: the write
    proceeds and no degraded record is produced."""
    calls: list[str] = []

    def _boom(_path: Path | str) -> SpaceProbe:
        raise OSError("cannot stat")

    preflight = DiskPreflight(probe=_boom)
    result = preflight.guard("/gone", lambda: calls.append("wrote") or "ok")

    assert result == "ok"
    assert calls == ["wrote"]
    assert preflight.records == []


# --- telemetry mirror + real probe ------------------------------------------


def test_degraded_record_mirrors_to_telemetry_sink() -> None:
    class _Sink:
        def __init__(self) -> None:
            self.records: list[TelemetryRecord] = []

        def append_telemetry(self, record: TelemetryRecord) -> None:
            self.records.append(record)

    sink = _Sink()
    preflight = DiskPreflight(
        probe=_fixed_probe(free_bytes=1, free_inodes=AMPLE_INODES),
        sink=sink,
    )

    preflight.check("/x", run_id="run-1")

    assert len(sink.records) == 1
    emitted = sink.records[0]
    assert emitted.kind == DEGRADED_SPACE_KIND
    assert emitted.run_id == "run-1"
    assert emitted.payload["below_bytes"] is True


def test_probe_path_returns_positive_capacity(tmp_path: Path) -> None:
    """The real probe reports positive free bytes/inodes for a live path."""
    probe = probe_path(tmp_path)
    assert probe.free_bytes > 0
    assert probe.free_inodes > 0


def test_default_thresholds_are_modest() -> None:
    """Defaults are small enough that a healthy host never trips them."""
    assert DEFAULT_MIN_FREE_BYTES == 32 * 1024 * 1024
    assert DEFAULT_MIN_FREE_INODES == 1024
    assert DiskThreshold().min_free_bytes == DEFAULT_MIN_FREE_BYTES
    assert DiskThreshold().min_free_inodes == DEFAULT_MIN_FREE_INODES

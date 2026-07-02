"""Disk/inode preflight for authoritative store writes (spec: phase 07).

An authoritative store write that runs out of bytes or inodes mid-flush
hard-crashes on ``ENOSPC`` and can leave the store row half-written. This
module gives the worker a cheap, guarded probe it runs *ahead* of such a
write: when free space or free inodes are below the configured threshold it
records a queryable :class:`~flywheel_core.store_protocols.DegradedSpaceRecord`
and declines the write, rather than attempting it and catching the crash after
(which is what would leave torn state).

The probe (``shutil.disk_usage`` for bytes, ``os.statvfs`` for inodes) lives
here, in the worker layer — never in ``flywheel_core.task`` / ``.lifecycle``,
which stay pure. The record type it produces is the pure data shape defined in
``flywheel_core.store_protocols`` so the decline surfaces on the same
status/telemetry surface every other store record does.

Fail-open by construction: if the probe itself cannot run (an unstattable
path, a platform without ``os.statvfs``), the guard proceeds with the write —
inability to *measure* free space must never be mistaken for *lack* of it and
silently drop an authoritative write.
"""

from __future__ import annotations

import os
import shutil
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, TypeVar

from flywheel_core.store_protocols import (
    DegradedSpaceRecord,
    TelemetryRecord,
    TelemetrySink,
)

T = TypeVar("T")

Logger = Callable[[str], None]

# Default thresholds. Deliberately modest — magnitude is not load-bearing, only
# the below/above split is. On any healthy host there are orders of magnitude
# more than 32 MiB and 1024 inodes free, so a default-on preflight never blocks
# normal operation; the values only trip when a disk is genuinely near-full.
DEFAULT_MIN_FREE_BYTES: int = 32 * 1024 * 1024
DEFAULT_MIN_FREE_INODES: int = 1024

# Telemetry kind emitted (best-effort) when the preflight declines a write, so
# the decline also appears in a run's telemetry stream when a sink is wired.
DEGRADED_SPACE_KIND: str = "worker.disk_space_degraded"


@dataclass(frozen=True)
class SpaceProbe:
    """Observed free capacity on a filesystem: bytes and inodes.

    ``free_inodes`` is ``sys.maxsize`` on platforms/filesystems that do not
    report an inode count (``os.statvfs`` absent or ``f_favail == 0`` with no
    total), so the inode dimension simply never trips there rather than
    spuriously degrading.
    """

    free_bytes: int
    free_inodes: int


@dataclass(frozen=True)
class DiskThreshold:
    """Configurable floor below which a filesystem is considered degraded.

    A write is declined when free bytes fall below ``min_free_bytes`` OR free
    inodes fall below ``min_free_inodes`` — either dimension is sufficient.
    """

    min_free_bytes: int = DEFAULT_MIN_FREE_BYTES
    min_free_inodes: int = DEFAULT_MIN_FREE_INODES


def probe_path(path: Path | str) -> SpaceProbe:
    """Probe free bytes (``shutil.disk_usage``) and free inodes
    (``os.statvfs``) for the filesystem backing ``path``.

    Raises whatever ``shutil.disk_usage`` raises for an unstattable path
    (e.g. ``FileNotFoundError``); callers that must fail open catch it. Inode
    reporting degrades gracefully: a platform without ``os.statvfs`` (Windows)
    or a filesystem that reports no inode total yields ``sys.maxsize`` free
    inodes, disabling the inode dimension rather than tripping it.
    """
    usage = shutil.disk_usage(os.fspath(path))
    free_inodes = _probe_free_inodes(os.fspath(path))
    return SpaceProbe(free_bytes=usage.free, free_inodes=free_inodes)


def _probe_free_inodes(path: str) -> int:
    statvfs = getattr(os, "statvfs", None)
    if statvfs is None:
        return sys.maxsize
    try:
        st = statvfs(path)
    except OSError:
        return sys.maxsize
    # A filesystem with no inode concept reports f_files == 0; treat its inode
    # dimension as unlimited so it never trips.
    if st.f_files == 0:
        return sys.maxsize
    return st.f_favail


def classify(
    probe: SpaceProbe,
    threshold: DiskThreshold,
    *,
    path: Path | str,
    now: datetime,
    run_id: str | None = None,
) -> DegradedSpaceRecord | None:
    """Return a :class:`DegradedSpaceRecord` when ``probe`` is below either
    dimension of ``threshold``, else ``None`` (healthy — proceed with the
    write).
    """
    below_bytes = probe.free_bytes < threshold.min_free_bytes
    below_inodes = probe.free_inodes < threshold.min_free_inodes
    if not (below_bytes or below_inodes):
        return None
    return DegradedSpaceRecord(
        path=os.fspath(path),
        free_bytes=probe.free_bytes,
        free_inodes=probe.free_inodes,
        min_free_bytes=threshold.min_free_bytes,
        min_free_inodes=threshold.min_free_inodes,
        below_bytes=below_bytes,
        below_inodes=below_inodes,
        ts=now,
        run_id=run_id,
    )


@dataclass
class DiskPreflight:
    """Guards an authoritative store write behind a disk/inode probe.

    Default-on and configurable: constructed with no arguments it uses
    :class:`DiskThreshold`'s defaults and the real :func:`probe_path`. Tests
    (and callers wanting a different floor) inject a ``threshold`` and/or a
    ``probe`` callable.

    The declined records accumulate on :attr:`records` — the queryable status
    surface a caller reads back — and are mirrored to :attr:`sink` as a
    :class:`~flywheel_core.store_protocols.TelemetryRecord` when one is wired,
    so the decline also lands in the run's telemetry stream.
    """

    threshold: DiskThreshold = field(default_factory=DiskThreshold)
    probe: Callable[[Path | str], SpaceProbe] = probe_path
    clock: Callable[[], datetime] = lambda: datetime.now(timezone.utc)
    sink: TelemetrySink | None = None
    log: Logger | None = None
    records: list[DegradedSpaceRecord] = field(default_factory=list)

    def check(
        self, path: Path | str, *, run_id: str | None = None
    ) -> DegradedSpaceRecord | None:
        """Probe ``path`` and, if degraded, record + return the witness;
        otherwise return ``None``.

        Fail-open: a probe that raises (unstattable path, missing
        ``os.statvfs``) is treated as "cannot measure, so do not block" and
        returns ``None`` — the write proceeds.
        """
        try:
            probe = self.probe(path)
        except OSError as exc:
            if self.log is not None:
                self.log(
                    f"disk preflight could not probe {os.fspath(path)!r} "
                    f"({type(exc).__name__}: {exc}); proceeding with write"
                )
            return None
        record = classify(
            probe,
            self.threshold,
            path=path,
            now=self.clock(),
            run_id=run_id,
        )
        if record is not None:
            self._record(record)
        return record

    def guard(
        self,
        path: Path | str,
        do_write: Callable[[], T],
        *,
        run_id: str | None = None,
    ) -> T | None:
        """Run ``do_write`` only when ``path`` has adequate free space/inodes.

        Returns ``do_write()``'s result when healthy. When degraded it records
        the witness, skips ``do_write`` entirely (so no torn store row is
        possible), and returns ``None``.
        """
        if self.check(path, run_id=run_id) is not None:
            return None
        return do_write()

    def _record(self, record: DegradedSpaceRecord) -> None:
        self.records.append(record)
        if self.log is not None:
            dims = ", ".join(
                d
                for d, tripped in (
                    ("bytes", record.below_bytes),
                    ("inodes", record.below_inodes),
                )
                if tripped
            )
            self.log(
                f"disk preflight: declining authoritative store write for "
                f"{record.path!r} (degraded: {dims}; "
                f"free_bytes={record.free_bytes} < {record.min_free_bytes}, "
                f"free_inodes={record.free_inodes} < {record.min_free_inodes})"
            )
        if self.sink is not None:
            self._mirror_to_sink(record)

    def _mirror_to_sink(self, record: DegradedSpaceRecord) -> None:
        if self.sink is None:
            return
        self.sink.append_telemetry(
            TelemetryRecord(
                run_id=record.run_id or "",
                ts=record.ts,
                kind=DEGRADED_SPACE_KIND,
                payload={
                    "path": record.path,
                    "free_bytes": record.free_bytes,
                    "free_inodes": record.free_inodes,
                    "min_free_bytes": record.min_free_bytes,
                    "min_free_inodes": record.min_free_inodes,
                    "below_bytes": record.below_bytes,
                    "below_inodes": record.below_inodes,
                },
            )
        )


__all__ = [
    "DEFAULT_MIN_FREE_BYTES",
    "DEFAULT_MIN_FREE_INODES",
    "DEGRADED_SPACE_KIND",
    "DiskPreflight",
    "DiskThreshold",
    "SpaceProbe",
    "classify",
    "probe_path",
]

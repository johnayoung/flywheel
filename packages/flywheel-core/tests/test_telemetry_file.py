"""Tests for ``flywheel_core.telemetry_file.FileTelemetrySink``.

Pins the FR-2 contract: one append-only JSONL file per run at
``<logs_root>/runs/<run_id>.jsonl``, one flushed JSON object per line,
file write order canonical, concurrent runs disjoint, and idempotent
``runs/`` directory creation.
"""

from __future__ import annotations

import json
import threading
from datetime import datetime, timezone
from pathlib import Path

import pytest

from flywheel_core import (
    DEFAULT_LOGS_ROOT,
    FileTelemetrySink,
    TelemetryRecord,
    TelemetrySink,
)


def _record(
    run_id: str = "run-a",
    *,
    kind: str = "message_turn",
    payload: dict[str, object] | None = None,
    attempt_number: int | None = 1,
    iteration_number: int | None = 1,
    ts: datetime | None = None,
) -> TelemetryRecord:
    return TelemetryRecord(
        run_id=run_id,
        ts=ts or datetime(2026, 6, 11, 12, 0, 0, tzinfo=timezone.utc),
        kind=kind,
        payload=payload if payload is not None else {"text": "hello"},
        attempt_number=attempt_number,
        iteration_number=iteration_number,
    )


def _lines(path: Path) -> list[dict[str, object]]:
    raw = path.read_text(encoding="utf-8")
    assert raw.endswith("\n")
    return [json.loads(line) for line in raw.splitlines()]


# --- Protocol conformance ---------------------------------------------------


def test_file_sink_satisfies_telemetry_sink_protocol(tmp_path: Path) -> None:
    assert isinstance(FileTelemetrySink(tmp_path), TelemetrySink)


def test_default_logs_root_is_dot_flywheel_logs() -> None:
    assert DEFAULT_LOGS_ROOT == Path(".flywheel/logs")
    sink = FileTelemetrySink()
    assert sink.path_for("run-x") == Path(".flywheel/logs/runs/run-x.jsonl")


# --- File layout ------------------------------------------------------------


def test_append_writes_jsonl_file_under_runs_dir(tmp_path: Path) -> None:
    with FileTelemetrySink(tmp_path) as sink:
        sink.append_telemetry(_record("run-1"))
    path = tmp_path / "runs" / "run-1.jsonl"
    assert path.is_file()
    assert _lines(path) == [
        {
            "kind": "message_turn",
            "ts": "2026-06-11T12:00:00+00:00",
            "run_id": "run-1",
            "attempt_number": 1,
            "iteration_number": 1,
            "payload": {"text": "hello"},
        }
    ]


def test_lines_appear_in_emission_order(tmp_path: Path) -> None:
    with FileTelemetrySink(tmp_path) as sink:
        for n in range(50):
            sink.append_telemetry(
                _record("run-1", kind="tick", payload={"n": n})
            )
    payloads = [line["payload"] for line in _lines(sink.path_for("run-1"))]
    assert payloads == [{"n": n} for n in range(50)]


def test_each_line_is_flushed_before_append_returns(tmp_path: Path) -> None:
    sink = FileTelemetrySink(tmp_path)
    try:
        sink.append_telemetry(_record("run-1", payload={"n": 0}))
        # Read while the sink still holds its handle open: the line must
        # already be visible on disk.
        assert _lines(sink.path_for("run-1"))[0]["payload"] == {"n": 0}
    finally:
        sink.close()


def test_none_coordinates_serialize_as_null(tmp_path: Path) -> None:
    with FileTelemetrySink(tmp_path) as sink:
        sink.append_telemetry(
            _record("run-1", attempt_number=None, iteration_number=None)
        )
    (line,) = _lines(sink.path_for("run-1"))
    assert line["attempt_number"] is None
    assert line["iteration_number"] is None


# --- Append-only across sink instances --------------------------------------


def test_new_sink_appends_to_existing_file_without_rewriting(
    tmp_path: Path,
) -> None:
    with FileTelemetrySink(tmp_path) as sink:
        sink.append_telemetry(_record("run-1", payload={"n": 0}))
    with FileTelemetrySink(tmp_path) as sink:
        sink.append_telemetry(_record("run-1", payload={"n": 1}))
    payloads = [line["payload"] for line in _lines(sink.path_for("run-1"))]
    assert payloads == [{"n": 0}, {"n": 1}]


def test_close_is_idempotent_and_append_reopens(tmp_path: Path) -> None:
    sink = FileTelemetrySink(tmp_path)
    sink.append_telemetry(_record("run-1", payload={"n": 0}))
    sink.close()
    sink.close()
    sink.append_telemetry(_record("run-1", payload={"n": 1}))
    sink.close()
    payloads = [line["payload"] for line in _lines(sink.path_for("run-1"))]
    assert payloads == [{"n": 0}, {"n": 1}]


# --- Concurrent runs --------------------------------------------------------


def test_concurrent_runs_write_disjoint_files(tmp_path: Path) -> None:
    with FileTelemetrySink(tmp_path) as sink:

        def emit(run_id: str) -> None:
            for n in range(25):
                sink.append_telemetry(
                    _record(run_id, kind="tick", payload={"n": n})
                )

        threads = [
            threading.Thread(target=emit, args=(run_id,))
            for run_id in ("run-a", "run-b")
        ]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

    for run_id in ("run-a", "run-b"):
        lines = _lines(sink.path_for(run_id))
        assert [line["run_id"] for line in lines] == [run_id] * 25
        assert [line["payload"] for line in lines] == [
            {"n": n} for n in range(25)
        ]


def test_two_sinks_race_on_first_append_in_same_root(tmp_path: Path) -> None:
    # The runs/ directory must be created idempotently even when two
    # sink instances append for the first time concurrently.
    barrier = threading.Barrier(2)
    errors: list[BaseException] = []

    def emit(run_id: str) -> None:
        try:
            with FileTelemetrySink(tmp_path) as sink:
                barrier.wait(timeout=5)
                sink.append_telemetry(_record(run_id))
        except BaseException as exc:  # pragma: no cover - failure detail
            errors.append(exc)

    threads = [
        threading.Thread(target=emit, args=(run_id,))
        for run_id in ("run-a", "run-b")
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert errors == []
    assert (tmp_path / "runs" / "run-a.jsonl").is_file()
    assert (tmp_path / "runs" / "run-b.jsonl").is_file()


# --- Run-id safety ----------------------------------------------------------


@pytest.mark.parametrize("bad", ["", ".", "..", "a/b", "a\\b", "../escape"])
def test_unsafe_run_id_is_rejected(tmp_path: Path, bad: str) -> None:
    sink = FileTelemetrySink(tmp_path)
    with pytest.raises(ValueError):
        sink.append_telemetry(_record(bad))
    with pytest.raises(ValueError):
        sink.path_for(bad)

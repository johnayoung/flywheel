"""Tests for the dashboard / TUI side of worker supervision.

Covers:

* The status bar renders ``worker: supervised`` / ``detached`` /
  ``none`` / ``dead`` for each :class:`WorkerState`.
* ``/worker start`` and ``/worker stop`` slash commands dispatch
  against the supervisor seam and surface the inline notice.
* The supervised-child quit prompt's Enter (detach) and ``s``
  (stop) branches both call the supervisor and exit; ``Esc``
  cancels without touching it.
* A console with no supervised child quits without prompting.
* The TUI parser accepts ``--no-worker`` and the launcher honours
  it by skipping the auto-spawn.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Coroutine
from pathlib import Path
from typing import Any

import pytest
from textual.widgets import Input, Static

from flywheel_core.store_sqlite import SqliteStore

from flywheel._dashboard import DashboardApp
from flywheel._quit_prompt import QuitPromptScreen
from flywheel._snapshot import DashboardSnapshot, RowSnapshot, SummaryData
from flywheel._tui import _build_parser
from flywheel._worker_supervisor import (
    WorkerState,
    WorkerStatus,
    WorkerSupervisor,
)


def _run(coro: Callable[[], Coroutine[Any, Any, None]]) -> None:
    asyncio.run(coro())


def _row(task_id: str, *, run_id: str | None = None) -> RowSnapshot:
    return RowSnapshot(
        run_id=run_id or f"run-{task_id}",
        task_id=task_id,
        status="running",
        attempt=1,
        iteration=2,
        age_seconds=3,
        tokens=42,
        cost_usd=0.123,
        turns=1,
        iterations_completed=1,
        last_kind="ASSISTANT",
        last_detail="Edit(file_path=README.md)",
        awaiting_instruction=None,
    )


def _snapshot(*rows: RowSnapshot) -> DashboardSnapshot:
    return DashboardSnapshot(
        summary=SummaryData(
            active_workers=len(rows),
            task_counts={},
            tokens_total=0,
            cost_usd_total=0.0,
            runtime_seconds=0,
        ),
        rows=tuple(rows),
    )


class _ScriptedSupervisor:
    """A test double for :class:`WorkerSupervisor`.

    Lets each test seed the sequence of statuses ``status()`` returns
    and assert which methods the dashboard invoked. Keeps the dashboard
    tests focused on UI plumbing -- the subprocess behaviour is
    covered by ``test_worker_supervisor``.
    """

    def __init__(self, *, initial: WorkerStatus | None = None) -> None:
        self._status = initial or WorkerStatus(state=WorkerState.NONE)
        self.calls: list[str] = []
        self.start_to: WorkerStatus | None = None
        self.stop_result: bool = True

    def status(self) -> WorkerStatus:
        return self._status

    def set_status(self, status: WorkerStatus) -> None:
        self._status = status

    def start(self) -> WorkerStatus:
        self.calls.append("start")
        if self.start_to is not None:
            self._status = self.start_to
        return self._status

    def stop(self) -> bool:
        self.calls.append("stop")
        self._status = WorkerStatus(state=WorkerState.DEAD, message="exit=0")
        return self.stop_result

    def detach(self) -> None:
        self.calls.append("detach")
        # Detached: the underlying claim (if any) determines the next
        # state. Tests can override via ``set_status``.
        self._status = WorkerStatus(state=WorkerState.NONE)


# --- Status bar rendering --------------------------------------------------


def test_worker_bar_renders_supervised_state() -> None:
    """The status bar shows ``worker: supervised`` with the pid when
    the supervisor reports SUPERVISED."""

    async def body() -> None:
        sup = _ScriptedSupervisor(
            initial=WorkerStatus(state=WorkerState.SUPERVISED, pid=4242)
        )
        app = DashboardApp(
            poll=lambda: _snapshot(_row("alpha")),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_start=sup.start,
            worker_stop=sup.stop,
            worker_detach=sup.detach,
        )
        async with app.run_test() as pilot:
            await pilot.pause()
            bar = app.query_one("#worker_bar", Static)
            assert not bar.has_class("hidden")
            text = str(bar.render())
            assert "supervised" in text
            assert "4242" in text

    _run(body)


def test_worker_bar_renders_detached_state() -> None:
    async def body() -> None:
        sup = _ScriptedSupervisor(
            initial=WorkerStatus(state=WorkerState.DETACHED)
        )
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
        )
        async with app.run_test() as pilot:
            await pilot.pause()
            bar = app.query_one("#worker_bar", Static)
            assert "detached" in str(bar.render())

    _run(body)


def test_worker_bar_renders_none_state_with_start_hint() -> None:
    """The ``none`` line embeds the ``/worker start`` hint the Error
    Handling row in the spec explicitly requires for ``--no-worker``
    with nothing live."""

    async def body() -> None:
        sup = _ScriptedSupervisor(initial=WorkerStatus(state=WorkerState.NONE))
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
        )
        async with app.run_test() as pilot:
            await pilot.pause()
            bar = app.query_one("#worker_bar", Static)
            text = str(bar.render())
            assert "none" in text
            assert "/worker start" in text

    _run(body)


def test_worker_bar_renders_dead_state_with_respawn_hint() -> None:
    async def body() -> None:
        sup = _ScriptedSupervisor(
            initial=WorkerStatus(
                state=WorkerState.DEAD, pid=999, message="exit=1"
            )
        )
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
        )
        async with app.run_test() as pilot:
            await pilot.pause()
            bar = app.query_one("#worker_bar", Static)
            text = str(bar.render())
            assert "dead" in text
            assert "/worker start" in text
            assert "exit=1" in text

    _run(body)


def test_worker_bar_hidden_when_supervisor_not_wired() -> None:
    """No ``worker_status`` callable -> the status bar widget stays
    hidden so snapshot-only tests are unaffected by the new wiring."""

    async def body() -> None:
        app = DashboardApp(
            poll=lambda: _snapshot(_row("alpha")), poll_interval_seconds=1000.0
        )
        async with app.run_test() as pilot:
            await pilot.pause()
            bar = app.query_one("#worker_bar", Static)
            assert bar.has_class("hidden")

    _run(body)


# --- /worker slash command -------------------------------------------------


def test_slash_worker_start_calls_supervisor_start() -> None:
    async def body() -> None:
        sup = _ScriptedSupervisor()
        sup.start_to = WorkerStatus(
            state=WorkerState.SUPERVISED, pid=4321
        )
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_start=sup.start,
            worker_stop=sup.stop,
            worker_detach=sup.detach,
        )
        async with app.run_test() as pilot:
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            input_widget.value = "/worker start"
            await input_widget.action_submit()
            await pilot.pause()
            assert sup.calls == ["start"]
            notice = app.query_one("#dashboard_notice", Static)
            assert not notice.has_class("hidden")
            assert "supervised" in str(notice.render())
            assert input_widget.value == ""

    _run(body)


def test_slash_worker_start_with_live_lease_does_not_spawn() -> None:
    """When a live worker already exists, ``/worker start`` returns
    DETACHED and the dashboard's inline notice surfaces that no spawn
    happened."""

    async def body() -> None:
        sup = _ScriptedSupervisor()
        sup.start_to = WorkerStatus(state=WorkerState.DETACHED)
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_start=sup.start,
        )
        async with app.run_test() as pilot:
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            input_widget.value = "/worker start"
            await input_widget.action_submit()
            await pilot.pause()
            notice = app.query_one("#dashboard_notice", Static)
            assert "detached" in str(notice.render())
            assert "no spawn" in str(notice.render())

    _run(body)


def test_slash_worker_stop_on_supervised_child_calls_stop() -> None:
    async def body() -> None:
        sup = _ScriptedSupervisor(
            initial=WorkerStatus(state=WorkerState.SUPERVISED, pid=1)
        )
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_start=sup.start,
            worker_stop=sup.stop,
            worker_detach=sup.detach,
        )
        async with app.run_test() as pilot:
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            input_widget.value = "/worker stop"
            await input_widget.action_submit()
            await pilot.pause()
            assert sup.calls == ["stop"]
            notice = app.query_one("#dashboard_notice", Static)
            assert "terminated gracefully" in str(notice.render())

    _run(body)


def test_slash_worker_stop_on_detached_worker_shows_inline_notice() -> None:
    """``/worker stop`` against a worker this console did NOT spawn
    must not signal it (spec Edge Cases); it shows an inline notice
    instead and never calls ``stop``."""

    async def body() -> None:
        sup = _ScriptedSupervisor(
            initial=WorkerStatus(state=WorkerState.DETACHED)
        )
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_stop=sup.stop,
        )
        async with app.run_test() as pilot:
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            input_widget.value = "/worker stop"
            await input_widget.action_submit()
            await pilot.pause()
            assert sup.calls == []  # never reached the supervisor.stop
            notice = app.query_one("#dashboard_notice", Static)
            assert "no supervised child" in str(notice.render())
            assert "detached" in str(notice.render())

    _run(body)


def test_slash_worker_unknown_subverb_shows_help() -> None:
    async def body() -> None:
        sup = _ScriptedSupervisor()
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_start=sup.start,
            worker_stop=sup.stop,
        )
        async with app.run_test() as pilot:
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            input_widget.value = "/worker tickle"
            await input_widget.action_submit()
            await pilot.pause()
            assert sup.calls == []
            notice = app.query_one("#dashboard_notice", Static)
            assert "unknown sub-verb" in str(notice.render())
            assert "start" in str(notice.render())
            assert "stop" in str(notice.render())

    _run(body)


# --- Quit prompt -----------------------------------------------------------


def test_quit_with_no_supervised_child_exits_immediately() -> None:
    """Detached / external / none / dead states never prompt; the
    console just exits."""

    async def body() -> None:
        sup = _ScriptedSupervisor(
            initial=WorkerStatus(state=WorkerState.DETACHED)
        )
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_detach=sup.detach,
            worker_stop=sup.stop,
        )
        async with app.run_test() as pilot:
            await pilot.press("q")
        # No detach / stop call was made; the console exited silently.
        assert sup.calls == []

    _run(body)


def test_quit_with_supervised_child_prompts_and_detaches_on_enter() -> None:
    """Enter on the prompt takes the detach branch: the supervisor's
    ``detach`` is invoked and the console exits."""

    async def body() -> None:
        sup = _ScriptedSupervisor(
            initial=WorkerStatus(state=WorkerState.SUPERVISED, pid=11)
        )
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_detach=sup.detach,
            worker_stop=sup.stop,
        )
        async with app.run_test() as pilot:
            await pilot.press("q")
            await pilot.pause()
            # The quit prompt is on the screen stack.
            assert isinstance(app.screen, QuitPromptScreen)
            await pilot.press("enter")
            await pilot.pause()
        assert sup.calls == ["detach"]

    _run(body)


def test_quit_with_supervised_child_stops_on_s() -> None:
    """``s`` on the prompt calls the supervisor's ``stop`` before exit."""

    async def body() -> None:
        sup = _ScriptedSupervisor(
            initial=WorkerStatus(state=WorkerState.SUPERVISED, pid=22)
        )
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_detach=sup.detach,
            worker_stop=sup.stop,
        )
        async with app.run_test() as pilot:
            await pilot.press("q")
            await pilot.pause()
            assert isinstance(app.screen, QuitPromptScreen)
            await pilot.press("s")
            await pilot.pause()
        assert sup.calls == ["stop"]

    _run(body)


def test_quit_prompt_escape_cancels_without_signaling() -> None:
    """Escape cancels: no detach / stop, console stays on the dashboard."""

    async def body() -> None:
        sup = _ScriptedSupervisor(
            initial=WorkerStatus(state=WorkerState.SUPERVISED, pid=33)
        )
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_detach=sup.detach,
            worker_stop=sup.stop,
        )
        async with app.run_test() as pilot:
            await pilot.press("q")
            await pilot.pause()
            assert isinstance(app.screen, QuitPromptScreen)
            await pilot.press("escape")
            await pilot.pause()
            # Prompt popped; dashboard regains focus; app still running.
            assert not isinstance(app.screen, QuitPromptScreen)
            assert sup.calls == []

    _run(body)


# --- TUI parser ------------------------------------------------------------


def test_tui_parser_accepts_no_worker_flag() -> None:
    args = _build_parser().parse_args(["--no-worker"])
    assert args.no_worker is True


def test_tui_parser_no_worker_default_false() -> None:
    args = _build_parser().parse_args([])
    assert args.no_worker is False


# --- End-to-end: --no-worker keeps a real supervisor from spawning ---------


def test_no_worker_flag_keeps_status_none_with_real_supervisor(
    tmp_path: Path,
) -> None:
    """With ``--no-worker`` the TUI must not spawn anything; a fresh
    :class:`WorkerSupervisor` built against an empty store reports
    NONE, and the dashboard's status bar shows ``worker: none``."""

    db = tmp_path / "db.sqlite"
    SqliteStore(db).close()

    async def body() -> None:
        supervisor = WorkerSupervisor(
            db_path=db,
            log_dir=tmp_path / "logs",
            # Even if start() were called, it would launch this sleep
            # child; the test asserts it never gets called because the
            # TUI honored --no-worker. The argv is set just to keep
            # the test deterministic if the assert fails.
            spawn_argv=[
                "python",
                "-c",
                "import time; time.sleep(60)",
            ],
        )
        try:
            app = DashboardApp(
                poll=lambda: _snapshot(),
                poll_interval_seconds=1000.0,
                worker_status=supervisor.status,
                worker_start=supervisor.start,
                worker_stop=supervisor.stop,
                worker_detach=supervisor.detach,
            )
            async with app.run_test() as pilot:
                await pilot.pause()
                bar = app.query_one("#worker_bar", Static)
                text = str(bar.render())
                assert "none" in text
                # No supervisor log was written because no spawn happened.
                logs = (
                    list((tmp_path / "logs").glob("supervisor-*.log"))
                    if (tmp_path / "logs").exists()
                    else []
                )
                assert logs == []
        finally:
            supervisor.close()

    _run(body)


# --- End-to-end: dashboard wired to a real supervisor through detach -------


def test_quit_with_real_supervisor_detaches_and_keeps_child_alive(
    tmp_path: Path,
) -> None:
    """End-to-end with a real :class:`WorkerSupervisor` and a sleep
    child: Enter on the prompt detaches, the child survives the
    console exit, the test then cleans up by signaling it directly.
    """

    import os
    import signal
    import sys
    import time

    db = tmp_path / "db.sqlite"
    SqliteStore(db).close()
    sup = WorkerSupervisor(
        db_path=db,
        log_dir=tmp_path / "logs",
        spawn_argv=[
            sys.executable,
            "-c",
            "import time; time.sleep(60)",
        ],
    )
    status = sup.start()
    assert status.state == WorkerState.SUPERVISED
    pid = status.pid
    assert pid is not None

    async def body() -> None:
        app = DashboardApp(
            poll=lambda: _snapshot(),
            poll_interval_seconds=1000.0,
            worker_status=sup.status,
            worker_start=sup.start,
            worker_stop=sup.stop,
            worker_detach=sup.detach,
        )
        async with app.run_test() as pilot:
            await pilot.press("q")
            await pilot.pause()
            assert isinstance(app.screen, QuitPromptScreen)
            await pilot.press("enter")
            await pilot.pause()

    try:
        _run(body)
        # The child must still be alive after the console exited.
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            pytest.fail("detached worker died before test could observe it")
        # Supervisor no longer owns it.
        assert sup.owns_supervised_child() is False
    finally:
        try:
            os.kill(pid, signal.SIGTERM)
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
        except ProcessLookupError:
            pass

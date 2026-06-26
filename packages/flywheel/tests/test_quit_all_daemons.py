"""Quitting the console stops BOTH supervised daemons, not just the worker.

Closing the console used to detach the worker via a prompt but detach the
autopilot daemon silently -- so an operator who quit (or pressed Ctrl-C) left a
detached autopilot running unawares. The quit handoff now accounts for both
supervised children, and ``/exit`` is the decisive "stop everything and leave".
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Coroutine
from typing import Any

from textual.widgets import Input

from flywheel._autopilot_supervisor import AutopilotState, AutopilotStatus
from flywheel._dashboard import DashboardApp
from flywheel._quit_prompt import QuitPromptScreen
from flywheel._snapshot import DashboardSnapshot, SummaryData
from flywheel._worker_supervisor import WorkerState, WorkerStatus


def _run(coro: Callable[[], Coroutine[Any, Any, None]]) -> None:
    asyncio.run(coro())


def _snapshot() -> DashboardSnapshot:
    return DashboardSnapshot(
        summary=SummaryData(
            active_workers=0,
            task_counts={},
            tokens_total=0,
            cost_usd_total=0.0,
            runtime_seconds=0,
        ),
        rows=(),
    )


class _ScriptedWorker:
    """Faithful double: stop/detach act (and record) only when it owns a live
    child -- exactly the real supervisor's no-op-when-not-supervised contract."""

    def __init__(self, *, live: bool) -> None:
        self._live = live
        self.calls: list[str] = []

    def status(self) -> WorkerStatus:
        state = WorkerState.SUPERVISED if self._live else WorkerState.NONE
        return WorkerStatus(state=state, pid=1 if self._live else None)

    def stop(self) -> bool:
        if not self._live:
            return False
        self.calls.append("stop")
        return True

    def detach(self) -> None:
        if self._live:
            self.calls.append("detach")


class _ScriptedAutopilot:
    def __init__(self, *, live: bool) -> None:
        self._live = live
        self.calls: list[str] = []

    def status(self) -> AutopilotStatus:
        state = AutopilotState.SUPERVISED if self._live else AutopilotState.NONE
        return AutopilotStatus(state=state, pid=2 if self._live else None)

    def stop(self) -> bool:
        if not self._live:
            return False
        self.calls.append("stop")
        return True

    def detach(self) -> None:
        if self._live:
            self.calls.append("detach")


def _app(worker: _ScriptedWorker, autopilot: _ScriptedAutopilot) -> DashboardApp:
    return DashboardApp(
        poll=lambda: _snapshot(),
        poll_interval_seconds=1000.0,
        worker_status=worker.status,
        worker_stop=worker.stop,
        worker_detach=worker.detach,
        autopilot_status=autopilot.status,
        autopilot_stop=autopilot.stop,
        autopilot_detach=autopilot.detach,
    )


# --- the quit prompt covers both daemons ------------------------------------


def test_quit_prompts_when_only_autopilot_is_supervised() -> None:
    """Autopilot alone (no worker) now triggers the prompt -- not a silent
    detach. Stopping takes autopilot down."""

    async def body() -> None:
        worker = _ScriptedWorker(live=False)
        autopilot = _ScriptedAutopilot(live=True)
        app = _app(worker, autopilot)
        async with app.run_test() as pilot:
            await pilot.press("q")
            await pilot.pause()
            assert isinstance(app.screen, QuitPromptScreen)
            await pilot.press("s")
            await pilot.pause()
        assert autopilot.calls == ["stop"]
        assert worker.calls == []  # worker was not live, never signaled

    _run(body)


def test_quit_stop_takes_down_both_daemons() -> None:
    async def body() -> None:
        worker = _ScriptedWorker(live=True)
        autopilot = _ScriptedAutopilot(live=True)
        app = _app(worker, autopilot)
        async with app.run_test() as pilot:
            await pilot.press("q")
            await pilot.pause()
            assert isinstance(app.screen, QuitPromptScreen)
            await pilot.press("s")
            await pilot.pause()
        assert worker.calls == ["stop"]
        assert autopilot.calls == ["stop"]

    _run(body)


def test_quit_detach_detaches_both_daemons() -> None:
    async def body() -> None:
        worker = _ScriptedWorker(live=True)
        autopilot = _ScriptedAutopilot(live=True)
        app = _app(worker, autopilot)
        async with app.run_test() as pilot:
            await pilot.press("q")
            await pilot.pause()
            assert isinstance(app.screen, QuitPromptScreen)
            await pilot.press("enter")
            await pilot.pause()
        assert worker.calls == ["detach"]
        assert autopilot.calls == ["detach"]

    _run(body)


def test_quit_no_daemons_exits_without_prompt() -> None:
    async def body() -> None:
        worker = _ScriptedWorker(live=False)
        autopilot = _ScriptedAutopilot(live=False)
        app = _app(worker, autopilot)
        async with app.run_test() as pilot:
            await pilot.press("q")
            await pilot.pause()
            # No prompt; the app exits.
        assert worker.calls == [] and autopilot.calls == []

    _run(body)


# --- /exit stops everything immediately, no prompt --------------------------


def test_slash_exit_stops_all_daemons_and_exits() -> None:
    async def body() -> None:
        worker = _ScriptedWorker(live=True)
        autopilot = _ScriptedAutopilot(live=True)
        app = _app(worker, autopilot)
        async with app.run_test() as pilot:
            input_widget = app.query_one("#dashboard_input", Input)
            input_widget.focus()
            await pilot.pause()
            input_widget.value = "/exit"
            await input_widget.action_submit()
            await pilot.pause()
            # No prompt -- /exit is decisive.
            assert not isinstance(app.screen, QuitPromptScreen)
        assert worker.calls == ["stop"]
        assert autopilot.calls == ["stop"]

    _run(body)

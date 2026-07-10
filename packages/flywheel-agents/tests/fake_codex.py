"""Deterministic fake codex CLI for flywheel-agents tests.

Emits ``codex exec --json`` JSONL lines on stdout, selected by the
``FAKE_CODEX_SCENARIO`` environment variable (the real code path delivers it
via ``AgentConfiguration.environment`` -> ``LocalHost`` merge). The prompt
arrives piped on stdin and is consumed fully before any output; every argv
flag the adapter appends (``exec``, ``--json``, ...) is ignored.

Not a pytest module: run it as a script (``python fake_codex.py``).
"""

from __future__ import annotations

import json
import os
import sys
from collections.abc import Callable
from typing import Any

THREAD_ID = "codex-1"
HAPPY_TEXT_ONE = "Hello from codex. "
HAPPY_TEXT_TWO = "All done."
TURN_USAGE: dict[str, int] = {
    "input_tokens": 11,
    "cached_input_tokens": 4,
    "output_tokens": 3,
}
CRASH_STDERR = "fake-codex: fatal explosion"
TURN_FAILED_MESSAGE = "model stream disconnected"
# Matches the flywheel LOOP_STATUS envelope fence (docs/loop.md).
ENVELOPE_TEXT = (
    "Work is committed and ready for grading.\n"
    "<!-- LOOP_STATUS -->\n"
    '{"intent": "verify", "reason": "implementation complete"}\n'
    "<!-- /LOOP_STATUS -->"
)


def _emit(obj: dict[str, Any]) -> None:
    print(json.dumps(obj), flush=True)


def _thread_started(thread_id: str = THREAD_ID) -> None:
    _emit({"type": "thread.started", "thread_id": thread_id})


def _turn_started() -> None:
    _emit({"type": "turn.started"})


def _item(phase: str, item: dict[str, Any]) -> None:
    _emit({"type": f"item.{phase}", "item": item})


def _agent_message(item_id: str, text: str) -> dict[str, Any]:
    return {"type": "agent_message", "id": item_id, "text": text}


def _turn_completed(usage: dict[str, int] | None = None) -> None:
    obj: dict[str, Any] = {"type": "turn.completed"}
    if usage is not None:
        obj["usage"] = usage
    _emit(obj)


def _scenario_happy() -> int:
    _thread_started()
    _turn_started()
    _item("completed", _agent_message("msg-1", HAPPY_TEXT_ONE))
    _item(
        "started",
        {"type": "command_execution", "id": "cmd-1", "command": "echo hi"},
    )
    _item(
        "completed",
        {
            "type": "command_execution",
            "id": "cmd-1",
            "command": "echo hi",
            "aggregated_output": "hi\n",
            "exit_code": 0,
            "status": "completed",
        },
    )
    _item(
        "completed",
        {
            "type": "file_change",
            "id": "fc-1",
            "status": "completed",
            "changes": [{"path": "src/app.py", "kind": "update"}],
        },
    )
    _item(
        "completed",
        {
            "type": "todo_list",
            "id": "todo-1",
            "items": [{"text": "ship it", "completed": True}],
        },
    )
    _item("completed", _agent_message("msg-2", HAPPY_TEXT_TWO))
    _turn_completed(TURN_USAGE)
    return 0


def _scenario_tool_error() -> int:
    _thread_started()
    _turn_started()
    _item(
        "started",
        {"type": "command_execution", "id": "cmd-err", "command": "false"},
    )
    _item(
        "completed",
        {
            "type": "command_execution",
            "id": "cmd-err",
            "command": "false",
            "aggregated_output": "boom",
            "exit_code": 2,
            "status": "failed",
        },
    )
    _turn_completed()
    return 0


def _scenario_turn_failed() -> int:
    _thread_started()
    _turn_started()
    _item("completed", _agent_message("msg-1", "partial answer"))
    _emit({"type": "turn.failed", "error": {"message": TURN_FAILED_MESSAGE}})
    return 0


def _scenario_malformed() -> int:
    _thread_started()
    _item("completed", _agent_message("msg-1", "before garbage "))
    print("this line is not json", flush=True)
    print("{not json", flush=True)
    print("[1,2,3]", flush=True)
    _emit({"type": "mystery", "x": 1})
    _item("completed", {"type": "quantum_flux", "id": "q-1"})
    _item("completed", _agent_message("msg-2", "after garbage"))
    _turn_completed(TURN_USAGE)
    return 0


def _scenario_crash() -> int:
    _thread_started()
    sys.stderr.write(CRASH_STDERR + "\n")
    sys.stderr.flush()
    return 3


def _scenario_envelope() -> int:
    _thread_started()
    _item("completed", _agent_message("msg-1", ENVELOPE_TEXT))
    _turn_completed(TURN_USAGE)
    return 0


def _scenario_cwd() -> int:
    _thread_started()
    _item("completed", _agent_message("msg-1", os.getcwd()))
    _turn_completed()
    return 0


def _scenario_env() -> int:
    _thread_started()
    report = json.dumps(
        {
            "marker": os.environ.get("TEST_MARKER"),
            "api_key": os.environ.get("OPENAI_API_KEY"),
        }
    )
    _item("completed", _agent_message("msg-1", report))
    _turn_completed()
    return 0


_SCENARIOS: dict[str, Callable[[], int]] = {
    "happy": _scenario_happy,
    "tool_error": _scenario_tool_error,
    "turn_failed": _scenario_turn_failed,
    "malformed": _scenario_malformed,
    "crash": _scenario_crash,
    "envelope": _scenario_envelope,
    "cwd": _scenario_cwd,
    "env": _scenario_env,
}


def main() -> int:
    sys.stdin.read()  # the prompt is piped; consume it fully before emitting
    scenario = os.environ.get("FAKE_CODEX_SCENARIO", "happy")
    handler = _SCENARIOS.get(scenario)
    if handler is None:
        sys.stderr.write(f"fake_codex: unknown scenario {scenario!r}\n")
        return 64
    return handler()


if __name__ == "__main__":
    sys.exit(main())

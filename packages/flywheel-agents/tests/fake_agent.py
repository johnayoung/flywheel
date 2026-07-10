"""Deterministic fake vendor CLI for flywheel-agents tests.

Emits Claude Code stream-json lines on stdout, selected by the
``FAKE_AGENT_SCENARIO`` environment variable (the real code path delivers it
via ``AgentConfiguration.environment`` -> ``LocalHost`` merge). The prompt
arrives piped on stdin and is consumed fully before any output; every argv
flag the adapter appends (``--print``, ``--output-format``, ...) is ignored.

Not a pytest module: run it as a script (``python fake_agent.py``).
"""

from __future__ import annotations

import json
import os
import sys
import time
from collections.abc import Callable
from typing import Any

ASSISTANT_USAGE: dict[str, int] = {
    "input_tokens": 11,
    "output_tokens": 4,
    "cache_creation_input_tokens": 1,
    "cache_read_input_tokens": 2,
}
RESULT_USAGE: dict[str, int] = {
    "input_tokens": 100,
    "output_tokens": 25,
    "cache_creation_input_tokens": 7,
    "cache_read_input_tokens": 3,
}
NUM_TURNS = 3
TOTAL_COST_USD = 0.0125
STDERR_MARKER = "FAKE-AGENT-STDERR-MARKER"
CRASH_STDERR = "fake-agent: fatal explosion"
USAGE_LIMIT_TEXT = "Claude AI usage limit reached|1751990400"


def _emit(obj: dict[str, Any]) -> None:
    print(json.dumps(obj), flush=True)


def _init(session_id: str = "sess-1") -> None:
    _emit(
        {
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": "claude-fake-1",
        }
    )


def _text(text: str) -> dict[str, Any]:
    return {"type": "text", "text": text}


def _tool_use(
    tool_use_id: str, name: str, tool_input: dict[str, Any]
) -> dict[str, Any]:
    return {"type": "tool_use", "id": tool_use_id, "name": name, "input": tool_input}


def _assistant(
    blocks: list[dict[str, Any]],
    *,
    usage: dict[str, int] | None = None,
    stop_reason: str = "end_turn",
) -> None:
    message: dict[str, Any] = {"content": blocks, "stop_reason": stop_reason}
    if usage is not None:
        message["usage"] = usage
    _emit({"type": "assistant", "message": message})


def _tool_result(tool_use_id: str, content: str, *, is_error: bool) -> None:
    _emit(
        {
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content,
                        "is_error": is_error,
                    }
                ]
            },
        }
    )


def _result(
    *,
    subtype: str = "success",
    is_error: bool = False,
    result_text: str = "done",
    num_turns: int = NUM_TURNS,
    total_cost_usd: float = TOTAL_COST_USD,
    usage: dict[str, int] | None = None,
) -> None:
    obj: dict[str, Any] = {
        "type": "result",
        "subtype": subtype,
        "is_error": is_error,
        "result": result_text,
        "num_turns": num_turns,
        "total_cost_usd": total_cost_usd,
    }
    if usage is not None:
        obj["usage"] = usage
    _emit(obj)


def _scenario_happy() -> int:
    _init()
    _assistant([_text("Hello, ")], usage=ASSISTANT_USAGE)
    _assistant(
        [_tool_use("tool-1", "Bash", {"command": "echo hi"})],
        stop_reason="tool_use",
    )
    _tool_result("tool-1", "hi", is_error=False)
    _assistant([_text("world.")])
    _result(result_text="Hello, world.", usage=RESULT_USAGE)
    return 0


def _scenario_tool_error() -> int:
    _init()
    _assistant(
        [_tool_use("tool-err", "Bash", {"command": "false"})],
        stop_reason="tool_use",
    )
    _tool_result("tool-err", "command failed", is_error=True)
    _result(result_text="tool failed")
    return 0


def _scenario_malformed() -> int:
    _init()
    _assistant([_text("before garbage ")])
    print("this line is not json", flush=True)
    print("{not json", flush=True)
    print("[1,2,3]", flush=True)
    _emit({"type": "mystery", "x": 1})
    _assistant([_text("after garbage")])
    _result(result_text="survived")
    return 0


def _scenario_crash() -> int:
    _init()
    _assistant([_text("about to crash")])
    sys.stderr.write(CRASH_STDERR + "\n")
    sys.stderr.flush()
    return 3


def _scenario_hang() -> int:
    _init()
    sys.stdout.flush()
    time.sleep(300)
    return 0


def _scenario_stderr() -> int:
    sys.stderr.write(STDERR_MARKER + "\n")
    sys.stderr.flush()
    _init()
    _assistant([_text("quiet run")])
    _result(result_text="quiet run")
    return 0


def _scenario_cwd() -> int:
    _init()
    _assistant([_text(os.getcwd())])
    _result(result_text="cwd reported")
    return 0


def _scenario_env() -> int:
    _init()
    report = json.dumps(
        {
            "marker": os.environ.get("TEST_MARKER"),
            "api_key": os.environ.get("ANTHROPIC_API_KEY"),
        }
    )
    _assistant([_text(report)])
    _result(result_text="environment reported")
    return 0


def _scenario_usage_limit() -> int:
    _init()
    _assistant([_text(USAGE_LIMIT_TEXT)])
    _result(
        subtype="error_during_execution",
        is_error=True,
        result_text=USAGE_LIMIT_TEXT,
    )
    return 0


def _scenario_no_result() -> int:
    _init()
    _assistant([_text("partial work")])
    return 0


_SCENARIOS: dict[str, Callable[[], int]] = {
    "happy": _scenario_happy,
    "tool_error": _scenario_tool_error,
    "malformed": _scenario_malformed,
    "crash": _scenario_crash,
    "hang": _scenario_hang,
    "stderr": _scenario_stderr,
    "cwd": _scenario_cwd,
    "env": _scenario_env,
    "usage_limit": _scenario_usage_limit,
    "no_result": _scenario_no_result,
}


def main() -> int:
    sys.stdin.read()  # the prompt is piped; consume it fully before emitting
    scenario = os.environ.get("FAKE_AGENT_SCENARIO", "happy")
    handler = _SCENARIOS.get(scenario)
    if handler is None:
        sys.stderr.write(f"fake_agent: unknown scenario {scenario!r}\n")
        return 64
    return handler()


if __name__ == "__main__":
    sys.exit(main())

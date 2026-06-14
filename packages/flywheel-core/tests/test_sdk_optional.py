"""``claude-agent-sdk`` is an optional extra, not an import-time dependency.

These tests run a fresh interpreter in which ``claude_agent_sdk`` is made
unimportable (a meta-path finder blocks it), proving that:

* ``import flywheel_core`` and ``import flywheel_orchestrator`` succeed with
  no agent SDK installed -- the whole data/lifecycle/store surface is usable
  by a consumer that never drives an agent;
* constructing a ``Task`` and parsing an envelope work in that environment;
* the failure is deferred to the agent-driving path and is branded
  (``MissingAgentSDKError``) with an actionable install hint.

Running in a subprocess is essential: the dev environment *has* the SDK
installed, so the only faithful way to test the SDK-absent contract is a
clean interpreter with the import blocked.
"""

from __future__ import annotations

import subprocess
import sys
import textwrap


def _run_without_sdk(body: str) -> subprocess.CompletedProcess[str]:
    """Execute ``body`` in a fresh interpreter where ``claude_agent_sdk`` is
    unimportable. Returns the completed process (caller asserts on it)."""
    program = textwrap.dedent(
        """
        import sys
        import importlib.abc
        import importlib.machinery

        class _Blocker(importlib.abc.MetaPathFinder):
            def find_spec(self, name, path, target=None):
                if name == "claude_agent_sdk" or name.startswith(
                    "claude_agent_sdk."
                ):
                    raise ModuleNotFoundError(
                        "No module named 'claude_agent_sdk'",
                        name="claude_agent_sdk",
                    )
                return None

        sys.meta_path.insert(0, _Blocker())
        # Drop any pre-imported copy so the blocker takes effect.
        for mod in list(sys.modules):
            if mod == "claude_agent_sdk" or mod.startswith("claude_agent_sdk."):
                del sys.modules[mod]
        """
    ) + textwrap.dedent(body)
    return subprocess.run(
        [sys.executable, "-c", program],
        text=True,
        capture_output=True,
        check=False,
    )


def test_core_and_orchestrator_import_without_sdk() -> None:
    result = _run_without_sdk(
        """
        import flywheel_core
        import flywheel_orchestrator
        from flywheel_core import Task, CommandGrader
        from flywheel_core.envelope import parse_envelope

        # The data surface works with no agent SDK present.
        t = Task(goal="g", graders=[CommandGrader(run="true")])
        t.validate()
        parse_envelope("no envelope here")

        # And the SDK really is unimportable in this interpreter.
        try:
            import claude_agent_sdk  # noqa: F401
        except ModuleNotFoundError:
            pass
        else:
            raise AssertionError("blocker failed: claude_agent_sdk imported")

        print("OK")
        """
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip().endswith("OK"), result.stdout


def test_agent_driving_path_raises_branded_error_without_sdk() -> None:
    result = _run_without_sdk(
        """
        from flywheel_core._sdk import MissingAgentSDKError, load_sdk

        try:
            load_sdk()
        except MissingAgentSDKError as exc:
            assert "flywheel-core[claude]" in str(exc), str(exc)
            print("OK")
        else:
            raise AssertionError("expected MissingAgentSDKError")
        """
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip().endswith("OK"), result.stdout


def test_sdk_attribute_access_raises_branded_error_without_sdk() -> None:
    result = _run_without_sdk(
        """
        import flywheel_core._sdk as sdk
        from flywheel_core._sdk import MissingAgentSDKError

        try:
            sdk.AssistantMessage
        except MissingAgentSDKError as exc:
            assert "claude" in str(exc).lower(), str(exc)
            print("OK")
        else:
            raise AssertionError("expected MissingAgentSDKError on attr access")
        """
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip().endswith("OK"), result.stdout

from pathlib import Path

import pytest

from flywheel_core import (
    CommandGrader,
    NoOpStrategy,
    Strategy,
    StrategyContext,
    StrategyResult,
    Task,
    derive_slug,
)


def _task() -> Task:
    return Task(
        id="t1",
        goal="trivial goal",
        graders=[CommandGrader(run="true")],
    )


def test_noop_strategy_implements_protocol() -> None:
    strategy: Strategy = NoOpStrategy()
    assert isinstance(strategy, Strategy)


def test_noop_setup_returns_typed_context_carrying_task_id() -> None:
    strategy = NoOpStrategy()
    ctx = strategy.setup(_task())
    assert isinstance(ctx, StrategyContext)
    assert ctx.task_id == "t1"
    assert ctx.ref == ""
    assert ctx.metadata == {}


def test_noop_submit_returns_deterministic_typed_result_with_empty_ref() -> None:
    strategy = NoOpStrategy()
    ctx = strategy.setup(_task())
    first = strategy.submit(ctx)
    second = strategy.submit(ctx)
    assert isinstance(first, StrategyResult)
    assert first == second
    assert first.ref == ""
    assert first.submitted is False


def test_noop_cleanup_is_safe_and_repeatable() -> None:
    strategy = NoOpStrategy()
    ctx = strategy.setup(_task())
    assert strategy.cleanup(ctx) is None
    assert strategy.cleanup(ctx) is None


def test_noop_strategy_performs_no_filesystem_side_effects(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.chdir(tmp_path)
    strategy = NoOpStrategy()
    ctx = strategy.setup(_task())
    strategy.submit(ctx)
    strategy.cleanup(ctx)
    assert list(tmp_path.iterdir()) == []


def test_derive_slug_strips_conventional_commit_prefix() -> None:
    assert derive_slug("feat: add login flow") == "add-login-flow"
    assert derive_slug("fix(auth): handle expired tokens") == "handle-expired-tokens"
    assert derive_slug("refactor!: drop deprecated api") == "drop-deprecated-api"


def test_derive_slug_uses_only_first_line() -> None:
    msg = "feat: add login flow\n\nLonger body that should be ignored."
    assert derive_slug(msg) == "add-login-flow"


def test_derive_slug_is_stable_for_same_input() -> None:
    msg = "fix: handle null user id"
    assert derive_slug(msg) == derive_slug(msg)


def test_derive_slug_handles_non_conventional_messages() -> None:
    assert derive_slug("Just a plain message") == "just-a-plain-message"


def test_derive_slug_total_on_empty_input_does_not_raise() -> None:
    assert derive_slug("") == "task"
    assert derive_slug("   ") == "task"
    assert derive_slug("\n\t  \n") == "task"


def test_derive_slug_total_on_empty_subject_after_prefix() -> None:
    assert derive_slug("feat:   ") == "task"
    assert derive_slug("chore:") == "task"


def test_derive_slug_total_on_punctuation_only_subject() -> None:
    assert derive_slug("feat: !!!") == "task"


def test_derive_slug_collapses_repeated_separators() -> None:
    assert derive_slug("feat: hello   world!! again") == "hello-world-again"

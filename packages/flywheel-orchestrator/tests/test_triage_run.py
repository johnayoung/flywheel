"""``flywheel triage`` entrypoint behavior (spec 00082, triage-verb-daemon).

Covers the contract the daemon shell owes around the (untouched) triage engine:
a malformed ``[triage]`` policy exits 2 *before* any GitHub write or even a git
call (the write-free guarantee), a non-github source is refused, ``--once`` runs
exactly one pass and exits 0, and the policy's ``max_per_pass`` reaches the
engine's ``per_pass_cap``.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from flywheel_orchestrator import _triage_run
from flywheel_orchestrator._triage import TriagePassResult

_GITHUB_SOURCE = """\
[source]
kind = "github"
repo = "octo/repo"
label = "flywheel"
"""


def _write_policy(tmp_path: Path, body: str) -> None:
    (tmp_path / "flywheel.toml").write_text(body, encoding="utf-8")


def _forbidden_runner(argv: object) -> str:  # pragma: no cover - must not run
    raise AssertionError(f"no gh call may be issued, got: {argv!r}")


def test_malformed_triage_policy_exits_two_write_free(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A bad ``[triage]`` value exits 2 having issued no git and no gh call.

    The write-free guarantee is the assertion, not merely the exit code: the
    default gh runner is never obtained (so no GitHub read or write happens) and
    the repo root is never resolved (so no git subprocess runs) -- the policy
    error is raised and handled before the engine is constructed.
    """
    _write_policy(
        tmp_path,
        _GITHUB_SOURCE + "\n[triage]\ninterval_seconds = -5\n",
    )
    monkeypatch.chdir(tmp_path)

    def _forbidden_repo_root() -> Path:  # pragma: no cover - must not run
        raise AssertionError("repo root must not be resolved on a policy error")

    monkeypatch.setattr(_triage_run, "_default_runner", _forbidden_runner)
    monkeypatch.setattr(_triage_run, "_repo_root", _forbidden_repo_root)

    assert _triage_run.main([]) == 2
    err = capsys.readouterr().err
    assert "policy error" in err
    assert "triage.interval_seconds" in err


def test_non_github_source_is_refused(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Triage needs a github board; a directory source exits 2 write-free."""
    _write_policy(tmp_path, '[source]\nkind = "directory"\n')
    monkeypatch.chdir(tmp_path)

    def _forbidden_repo_root() -> Path:  # pragma: no cover - must not run
        raise AssertionError("repo root must not be resolved without a board")

    monkeypatch.setattr(_triage_run, "_default_runner", _forbidden_runner)
    monkeypatch.setattr(_triage_run, "_repo_root", _forbidden_repo_root)

    assert _triage_run.main([]) == 2
    assert "github" in capsys.readouterr().err


def test_once_runs_exactly_one_pass_and_exits_zero(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``--once`` runs a single triage pass, wiring the defaults, and returns 0."""
    _write_policy(tmp_path, _GITHUB_SOURCE)
    monkeypatch.chdir(tmp_path)

    calls: list[dict[str, object]] = []

    def _fake_single_pass(**kwargs: object) -> TriagePassResult:
        calls.append(kwargs)
        return TriagePassResult()

    monkeypatch.setattr(_triage_run, "_repo_root", lambda: tmp_path)
    monkeypatch.setattr(_triage_run, "run_single_pass", _fake_single_pass)

    assert _triage_run.main(["--once"]) == 0
    assert len(calls) == 1
    kwargs = calls[0]
    assert kwargs["repo"] == "octo/repo"
    assert kwargs["intake_label"] == "flywheel"
    assert kwargs["ready_label"] == "flywheel:ready"
    assert kwargs["needs_detail_label"] == "flywheel:needs-detail"
    # No [triage] max_per_pass -> the engine's cap stays uncapped.
    assert kwargs["per_pass_cap"] is None


def test_once_wires_max_per_pass_into_the_engine_cap(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``[triage] max_per_pass`` reaches the engine's ``per_pass_cap`` verbatim."""
    _write_policy(
        tmp_path,
        _GITHUB_SOURCE + "\n[triage]\nmax_per_pass = 7\n",
    )
    monkeypatch.chdir(tmp_path)

    seen: dict[str, object] = {}

    def _fake_single_pass(**kwargs: object) -> TriagePassResult:
        seen.update(kwargs)
        return TriagePassResult()

    monkeypatch.setattr(_triage_run, "_repo_root", lambda: tmp_path)
    monkeypatch.setattr(_triage_run, "run_single_pass", _fake_single_pass)

    assert _triage_run.main(["--once"]) == 0
    assert seen["per_pass_cap"] == 7

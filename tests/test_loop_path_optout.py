"""Tests for ``flywheel.workflow.load_loop_path_optout``.

Covers the three documented outcomes from the FR-5 opt-out contract in
``.workflow/specs/00017-FEATURE-in-loop-verification-gate.md``:

* the artifact is absent -> the loader returns ``None``;
* the artifact is present with valid front-matter -> the loader returns
  the parsed :class:`LoopPathOptOut` record;
* the artifact is present but its front-matter is malformed or missing a
  required key -> the loader raises :class:`LoopPathOptOutError`, never
  silently passes an empty claim as valid.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from flywheel_orchestrator._workflow import (
    LOOP_PATH_OPTOUT_FILENAME,
    LoopPathOptOut,
    LoopPathOptOutError,
    load_loop_path_optout,
)


def _write_artifact(phase_dir: Path, body: str) -> Path:
    phase_dir.mkdir(parents=True, exist_ok=True)
    artifact = phase_dir / LOOP_PATH_OPTOUT_FILENAME
    artifact.write_text(body, encoding="utf-8")
    return artifact


# --- Absent -----------------------------------------------------------------


def test_absent_artifact_returns_none(tmp_path: Path) -> None:
    phase_dir = tmp_path / "active" / "19-no-optout"
    phase_dir.mkdir(parents=True)
    assert load_loop_path_optout(phase_dir) is None


def test_missing_phase_dir_returns_none(tmp_path: Path) -> None:
    # Probing a candidate path that does not exist is not an error -- the
    # opt-out simply does not exist there. Callers (the archive gate, the
    # audit re-check) may safely probe any phase dir.
    phase_dir = tmp_path / "active" / "does-not-exist"
    assert load_loop_path_optout(phase_dir) is None


def test_other_files_in_phase_dir_are_ignored(tmp_path: Path) -> None:
    # Only ``loop-path-exempt.md`` counts; an unrelated markdown file in
    # the phase dir does not trip the loader.
    phase_dir = tmp_path / "active" / "19-unrelated"
    phase_dir.mkdir(parents=True)
    (phase_dir / "notes.md").write_text("just notes\n", encoding="utf-8")
    assert load_loop_path_optout(phase_dir) is None


# --- Valid ------------------------------------------------------------------


def test_valid_frontmatter_returns_parsed_record(tmp_path: Path) -> None:
    phase_dir = tmp_path / "active" / "17-loop-path-gate"
    _write_artifact(
        phase_dir,
        "---\n"
        "phase: 17-loop-path-gate\n"
        "author: john\n"
        "reason: docstring fix only; diff adds no watched symbol\n"
        "---\n"
        "\n"
        "Free-form prose beyond the closing delimiter is ignored.\n",
    )
    assert load_loop_path_optout(phase_dir) == LoopPathOptOut(
        phase="17-loop-path-gate",
        author="john",
        reason="docstring fix only; diff adds no watched symbol",
    )


def test_blank_and_comment_lines_inside_block_are_tolerated(
    tmp_path: Path,
) -> None:
    phase_dir = tmp_path / "active" / "18-tolerant"
    _write_artifact(
        phase_dir,
        "---\n"
        "# emitted by /task on 2026-06-04\n"
        "phase: 18-tolerant\n"
        "\n"
        "author: jane\n"
        "reason: dependency bump only; no net symbol added\n"
        "---\n",
    )
    record = load_loop_path_optout(phase_dir)
    assert record is not None
    assert record.phase == "18-tolerant"
    assert record.author == "jane"
    assert record.reason == "dependency bump only; no net symbol added"


def test_value_containing_colon_is_preserved(tmp_path: Path) -> None:
    # The first ``:`` separates key from value; subsequent colons stay in
    # the value so reasons referencing ``module:line`` round-trip cleanly.
    phase_dir = tmp_path / "active" / "18-colons"
    _write_artifact(
        phase_dir,
        "---\n"
        "phase: 18-colons\n"
        "author: jane\n"
        "reason: refactor in src/flywheel/harness.py: rename only\n"
        "---\n",
    )
    record = load_loop_path_optout(phase_dir)
    assert record is not None
    assert (
        record.reason
        == "refactor in src/flywheel/harness.py: rename only"
    )


def test_unknown_keys_are_tolerated(tmp_path: Path) -> None:
    # Forward-compat: a future field must not break older loaders.
    phase_dir = tmp_path / "active" / "18-forward-compat"
    _write_artifact(
        phase_dir,
        "---\n"
        "phase: 18-forward-compat\n"
        "author: jane\n"
        "reason: comment-only edit\n"
        "ticket: PROJ-1234\n"
        "---\n",
    )
    record = load_loop_path_optout(phase_dir)
    assert record is not None
    assert record.phase == "18-forward-compat"


# --- Malformed --------------------------------------------------------------


def test_missing_required_key_raises(tmp_path: Path) -> None:
    phase_dir = tmp_path / "active" / "19-missing-reason"
    _write_artifact(
        phase_dir,
        "---\n"
        "phase: 19-missing-reason\n"
        "author: john\n"
        "---\n",
    )
    with pytest.raises(LoopPathOptOutError, match="reason"):
        load_loop_path_optout(phase_dir)


def test_empty_required_value_raises(tmp_path: Path) -> None:
    phase_dir = tmp_path / "active" / "19-empty-author"
    _write_artifact(
        phase_dir,
        "---\n"
        "phase: 19-empty-author\n"
        "author:\n"
        "reason: blank author should not pass\n"
        "---\n",
    )
    with pytest.raises(LoopPathOptOutError, match="author"):
        load_loop_path_optout(phase_dir)


def test_missing_opening_delimiter_raises(tmp_path: Path) -> None:
    phase_dir = tmp_path / "active" / "19-no-opening"
    _write_artifact(
        phase_dir,
        "phase: 19-no-opening\n"
        "author: john\n"
        "reason: missing leading delimiter\n",
    )
    with pytest.raises(LoopPathOptOutError, match="front-matter delimiter"):
        load_loop_path_optout(phase_dir)


def test_unclosed_frontmatter_raises(tmp_path: Path) -> None:
    phase_dir = tmp_path / "active" / "19-unclosed"
    _write_artifact(
        phase_dir,
        "---\n"
        "phase: 19-unclosed\n"
        "author: john\n"
        "reason: closing delimiter is missing\n",
    )
    with pytest.raises(LoopPathOptOutError, match="not closed"):
        load_loop_path_optout(phase_dir)


def test_malformed_line_without_colon_raises(tmp_path: Path) -> None:
    phase_dir = tmp_path / "active" / "19-no-colon"
    _write_artifact(
        phase_dir,
        "---\n"
        "phase: 19-no-colon\n"
        "author john\n"  # missing the ``:`` separator
        "reason: should reject\n"
        "---\n",
    )
    with pytest.raises(LoopPathOptOutError, match="expected 'key: value'"):
        load_loop_path_optout(phase_dir)


def test_empty_key_raises(tmp_path: Path) -> None:
    phase_dir = tmp_path / "active" / "19-empty-key"
    _write_artifact(
        phase_dir,
        "---\n"
        ": value-with-no-key\n"
        "phase: 19-empty-key\n"
        "author: john\n"
        "reason: should reject\n"
        "---\n",
    )
    with pytest.raises(LoopPathOptOutError, match="empty key"):
        load_loop_path_optout(phase_dir)


def test_empty_file_raises(tmp_path: Path) -> None:
    # A zero-byte opt-out file is the canonical "silently empty" case the
    # FR-5 contract refuses to accept as a valid claim.
    phase_dir = tmp_path / "active" / "19-empty-file"
    _write_artifact(phase_dir, "")
    with pytest.raises(LoopPathOptOutError):
        load_loop_path_optout(phase_dir)

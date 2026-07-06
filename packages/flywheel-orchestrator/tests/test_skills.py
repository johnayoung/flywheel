"""Tests for the Claude Code skill payload (``_skills``) and its
``flywheel init`` integration (the ``--skills`` flag and prompt)."""

from __future__ import annotations

import subprocess
import sys
import io
import re
from pathlib import Path

import pytest

from flywheel_orchestrator._policy import WorkPolicy
from flywheel_orchestrator._skills import (
    MANAGED_MARKER,
    SKILL_NAMES,
    SkillRenderSettings,
    SkillTemplateError,
    install_skills,
    render_skill,
    settings_from_policy,
)
from flywheel_orchestrator._workflow import main


@pytest.fixture
def repo(tmp_path: Path, monkeypatch) -> Path:
    # init's git preflight (spec 00028) refuses a non-git working dir, so
    # the fixture is a real attached-branch repo with one commit -- the
    # state a legitimate adopter running `flywheel init` is in.
    for args in (
        ("init", "-b", "main"),
        ("config", "user.email", "test@flywheel.invalid"),
        ("config", "user.name", "Flywheel Test"),
        ("commit", "--allow-empty", "-m", "root"),
    ):
        subprocess.run(
            ["git", *args], cwd=tmp_path, check=True, capture_output=True
        )
    monkeypatch.chdir(tmp_path)
    return tmp_path


class _FakeTty(io.StringIO):
    def isatty(self) -> bool:
        return True


def _interactive(monkeypatch, *lines: str) -> None:
    monkeypatch.setattr(
        sys, "stdin", _FakeTty("".join(f"{line}\n" for line in lines))
    )


# --- rendering ----------------------------------------------------------------


def test_every_skill_renders_with_default_settings() -> None:
    settings = SkillRenderSettings()
    for name in SKILL_NAMES:
        text = render_skill(name, settings)
        assert text.startswith("---\n"), name
        assert f"name: {name}" in text
        assert MANAGED_MARKER in text
        assert "__FW_" not in text, f"{name} leaked a placeholder"


def test_render_binds_default_paths() -> None:
    text = render_skill("fw-plan", SkillRenderSettings())
    assert ".flywheel/tasks/active/" in text
    assert ".flywheel/specs/" in text
    spec = render_skill("fw-spec", SkillRenderSettings())
    assert ".flywheel/specs/NNNNN-FEATURE-<name>.md" in spec
    retro = render_skill("fw-retro", SkillRenderSettings())
    assert ".flywheel/audits/" in retro
    assert ".flywheel/logs/runs/" in retro
    improve = render_skill("fw-improve", SkillRenderSettings())
    assert ".flywheel/proposals/" in improve
    verify = render_skill("fw-verify", SkillRenderSettings())
    assert ".flywheel/tasks/active/" in verify
    assert ".flywheel/specs/" in verify


def test_fw_verify_instructs_held_out_registration_and_keeps_fence() -> None:
    """Criterion #8 / D-4: the rendered fw-verify skill must BOTH instruct
    writing the admitted oracle's registration to the configured held-out
    root keyed by task id AND retain the fence forbidding committing the
    oracle into the tracked repo / wiring it as an in-repo command grader.
    The two are distinct channels and must coexist without contradiction."""
    text = render_skill("fw-verify", SkillRenderSettings())

    # (a) instructs registration at the held-out root keyed by task id.
    assert "[held_out] root" in text
    assert "<held-out-root>/<task_id>.json" in text
    # keyed by the owning task id, referencing the oracle by absolute path.
    assert "KEYED BY the owning task id" in text
    assert "ABSOLUTE OPERATOR PATH" in text

    # (b) retains the no-commit-into-repo / no-in-repo-grader fence.
    assert "wire it as an in-repo `command` grader" in text
    assert "never a commit into the tracked tree" in text
    # The fence and the registration are presented as DISTINCT moves.
    assert "DIFFERENT, sanctioned" in text

    # No leftover placeholder survives rendering.
    assert "__FW_" not in text


def test_flywheel_ops_is_shipped_and_renders_operability_runbook() -> None:
    """The sixth managed skill is the operability companion, not part of the
    authoring pipeline. It must ship in SKILL_NAMES, render marker-bearing,
    and carry the operability spine the run/drain/recover runbook needs. The
    surface-conformance holdout (every verb/topic is real) lives in the
    product suite (``flywheel/tests/test_ops_skill_surface.py``); here we only
    assert the template ships and reads as an operability runbook."""
    assert "flywheel-ops" in SKILL_NAMES
    text = render_skill("flywheel-ops", SkillRenderSettings())
    assert text.startswith("---\n")
    assert "name: flywheel-ops" in text
    assert MANAGED_MARKER in text
    assert "__FW_" not in text
    # Operability anchors: run/drain, evidence-derived status, and both
    # recovery shapes the skill exists to explain.
    assert "fw worker" in text
    assert "status --rollup" in text
    assert "parked" in text
    assert "stranded" in text
    # Depth is delegated to the curated docs, never duplicated inline.
    assert "fw docs" in text


def test_render_binds_custom_tasks_dir() -> None:
    settings = SkillRenderSettings(tasks_dir=Path("work/tasks"))
    text = render_skill("fw-plan", settings)
    assert "work/tasks/active/" in text
    assert ".flywheel/tasks" not in text
    # Conventional siblings anchor at the tasks dir's parent.
    assert "work/specs/" in text


def test_render_github_source_swaps_delivery_block() -> None:
    settings = SkillRenderSettings(
        source_kind="github",
        github_repo="octo/widgets",
        github_label="flywheel",
    )
    text = render_skill("fw-plan", settings)
    assert "gh issue create" in text
    assert "octo/widgets" in text
    assert "WORK ITEMS ARE GITHUB ISSUES" in text
    assert "CHOOSE PHASE DIRECTORY" not in text

    directory = render_skill("fw-plan", SkillRenderSettings())
    assert "gh issue create" not in directory
    assert "CHOOSE PHASE DIRECTORY" in directory


def test_render_unknown_name_raises() -> None:
    with pytest.raises(SkillTemplateError):
        render_skill("fw-nope", SkillRenderSettings())


def test_settings_from_policy_binds_policy_paths() -> None:
    policy = WorkPolicy(
        source_kind="directory",
        tasks_dir=Path("custom/tasks"),
        db_path=Path("custom/state.sqlite"),
    )
    settings = settings_from_policy(policy)
    assert settings.tasks_dir == Path("custom/tasks")
    assert settings.specs_dir == Path("custom/specs")
    assert settings.logs_dir == Path("custom/logs")
    # No policy -> the init defaults.
    assert settings_from_policy(None).tasks_dir == Path(".flywheel/tasks")


def test_settings_for_tracker_source_anchor_on_db_dir() -> None:
    policy = WorkPolicy(
        source_kind="github",
        github_repo="octo/widgets",
        github_label="flywheel",
        db_path=Path("state/flywheel.sqlite"),
    )
    settings = settings_from_policy(policy)
    assert settings.specs_dir == Path("state/specs")
    assert settings.audits_dir == Path("state/audits")


# --- install / regenerate semantics --------------------------------------------


def test_install_creates_then_updates_managed_files(tmp_path: Path) -> None:
    root = tmp_path / ".claude" / "skills"
    first = install_skills(root, SkillRenderSettings())
    assert len(first.created) == len(SKILL_NAMES)
    assert first.updated == ()
    assert first.skipped == ()
    for name in SKILL_NAMES:
        assert (root / name / "SKILL.md").is_file()

    # Re-install with changed settings regenerates in place.
    second = install_skills(
        root, SkillRenderSettings(tasks_dir=Path("work/tasks"))
    )
    assert second.created == ()
    assert len(second.updated) == len(SKILL_NAMES)
    assert "work/tasks/active/" in (
        root / "fw-plan" / "SKILL.md"
    ).read_text()


def test_install_skips_user_owned_files(tmp_path: Path) -> None:
    root = tmp_path / ".claude" / "skills"
    install_skills(root, SkillRenderSettings())
    target = root / "fw-plan" / "SKILL.md"
    # Removing the managed marker hands the file to the user.
    owned = target.read_text().replace(MANAGED_MARKER + "\n", "")
    target.write_text(owned)

    report = install_skills(
        root, SkillRenderSettings(tasks_dir=Path("work/tasks"))
    )
    assert target in report.skipped
    assert target.read_text() == owned
    # The other three are still managed and regenerated.
    assert len(report.updated) == len(SKILL_NAMES) - 1


# --- init integration -----------------------------------------------------------


def test_init_defaults_does_not_install_skills(repo: Path) -> None:
    assert main(["init", "--defaults"]) == 0
    assert not (repo / ".claude").exists()


def test_init_skills_flag_installs_non_interactively(
    repo: Path, capsys
) -> None:
    assert main(["init", "--defaults", "--skills"]) == 0
    out = capsys.readouterr().out
    for name in SKILL_NAMES:
        target = repo / ".claude" / "skills" / name / "SKILL.md"
        assert target.is_file()
        assert f"created: {Path('.claude/skills') / name / 'SKILL.md'}" in out
        assert "__FW_" not in target.read_text()


def test_init_no_skills_flag_suppresses_prompt(
    repo: Path, monkeypatch, capsys
) -> None:
    _interactive(monkeypatch, "", "")  # backend, source; no skills prompt
    assert main(["init", "--no-skills"]) == 0
    out = capsys.readouterr().out
    assert "install Claude Code skills" not in out
    assert not (repo / ".claude").exists()


def test_init_skills_render_against_the_written_policy(
    repo: Path, monkeypatch
) -> None:
    monkeypatch.setattr(
        "flywheel_orchestrator._workflow._github_repo_from_origin",
        lambda: "octo/widgets",
    )
    assert (
        main(
            [
                "init",
                "--source",
                "github",
                "--label",
                "work",
                "--defaults",
                "--skills",
            ]
        )
        == 0
    )
    plan = (
        repo / ".claude" / "skills" / "fw-plan" / "SKILL.md"
    ).read_text()
    assert "gh issue create" in plan
    assert "octo/widgets" in plan
    assert '--label "work"' in plan


def test_init_skills_on_existing_policy_regenerates_without_touching_it(
    repo: Path, capsys
) -> None:
    """The scriptable propagation path: edit flywheel.toml, then
    ``flywheel init --defaults --skills`` rebinds the managed skills."""
    assert main(["init", "--defaults", "--skills"]) == 0
    before_plan = (
        repo / ".claude" / "skills" / "fw-plan" / "SKILL.md"
    ).read_text()
    assert ".flywheel/tasks/active/" in before_plan

    policy_text = (repo / "flywheel.toml").read_text().replace(
        'tasks_dir = ".flywheel/tasks"', 'tasks_dir = "work/tasks"'
    )
    (repo / "flywheel.toml").write_text(policy_text)
    capsys.readouterr()

    assert main(["init", "--defaults", "--skills"]) == 0
    out = capsys.readouterr().out
    assert "exists:  flywheel.toml (left untouched)" in out
    assert "updated: " in out
    after_plan = (
        repo / ".claude" / "skills" / "fw-plan" / "SKILL.md"
    ).read_text()
    assert "work/tasks/active/" in after_plan


def test_init_reconfigure_regenerates_skills_with_new_source(
    repo: Path, monkeypatch
) -> None:
    assert main(["init", "--defaults", "--skills"]) == 0
    monkeypatch.setattr(
        "flywheel_orchestrator._workflow._github_repo_from_origin",
        lambda: "octo/widgets",
    )
    # reconfigure: yes, sqlite, github, repo default, label default,
    # done action default, skills yes
    _interactive(monkeypatch, "y", "", "github", "", "", "", "y")
    assert main(["init"]) == 0

    plan = (
        repo / ".claude" / "skills" / "fw-plan" / "SKILL.md"
    ).read_text()
    assert "gh issue create" in plan
    assert "octo/widgets" in plan


# --- docs drift guard ----------------------------------------------------------
#
# docs/workflow.md hand-maintains prose about the fw-* authoring skills, but the
# real source of truth is SKILL_NAMES / the _skill_templates. When a skill is
# renamed or added and the doc is not updated, the doc silently rots -- exactly
# how docs/workflow.md kept describing the removed /define and /task commands.
# These two tests fail CI on that drift instead.


def _find_repo_doc(relpath: str) -> Path | None:
    """Walk up from this test file to the repo root and return ``relpath``
    if it exists there. Returns None outside a source checkout."""
    for parent in Path(__file__).resolve().parents:
        candidate = parent / relpath
        if candidate.is_file():
            return candidate
    return None


def test_workflow_doc_documents_every_shipped_skill() -> None:
    doc_path = _find_repo_doc("docs/workflow.md")
    if doc_path is None:
        pytest.skip("docs/workflow.md not found (not a source checkout)")
    doc = doc_path.read_text(encoding="utf-8")
    missing = [name for name in SKILL_NAMES if name not in doc]
    assert not missing, (
        f"docs/workflow.md does not mention shipped skill(s): {missing}. "
        "Update the pipeline table and stage contracts when SKILL_NAMES changes."
    )


def test_workflow_doc_names_no_stale_fw_skill() -> None:
    doc_path = _find_repo_doc("docs/workflow.md")
    if doc_path is None:
        pytest.skip("docs/workflow.md not found (not a source checkout)")
    doc = doc_path.read_text(encoding="utf-8")
    referenced = set(re.findall(r"fw-[a-z]+", doc))
    unknown = referenced - set(SKILL_NAMES)
    assert not unknown, (
        f"docs/workflow.md references unknown fw-* skill(s): {sorted(unknown)}. "
        "A skill was renamed/removed, or the doc has a typo."
    )

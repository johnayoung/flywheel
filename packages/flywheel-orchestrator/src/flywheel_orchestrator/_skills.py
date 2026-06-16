"""Claude Code skill installation — ``flywheel init``'s optional payload.

``flywheel init`` can drop a set of agent skills into the repo's
``.claude/skills/`` so the authoring pipeline (idea -> spec -> tasks ->
held-out oracle -> retro -> proposals) is one slash command away in Claude Code:

* ``/fw-spec`` — interview an idea into ungameable, end-state success criteria
* ``/fw-plan`` — compile a spec or request into right-sized tasks, each spined
  on the strongest reward-hack-resistant grader the worker runs out-of-band
* ``/fw-verify`` — blind-author the discriminating held-out test oracle for
  those tasks before execute, so the agent never writes the test that grades it
* ``/fw-retro`` — forensic audit of how the loop executed a phase; every
  finding carries a re-runnable CLI pointer and stops at diagnosis
* ``/fw-improve`` — turn cited retro findings into ranked, scoped proposals,
  each ending in a handoff (``/fw-spec``, ``/fw-plan``, or accept)

The markdown lives as greenfield templates under ``_skill_templates/``
(package data) with ``__FW_*__`` placeholder tokens; rendering binds them
to the repo's actual settings (task directory, state directories, and —
for ``fw-plan`` — the work-source kind, so a GitHub-source repo gets
issue-creation instructions instead of task-file instructions).

Every generated ``SKILL.md`` carries :data:`MANAGED_MARKER` right after
its frontmatter. Re-running ``flywheel init`` (e.g. after changing the
tasks directory or the work source) regenerates marker-bearing files in
place; a file whose marker was removed is treated as user-owned and left
untouched. Names are ``fw-``-prefixed precisely to avoid colliding with
generic command names (``/task``, ``/define``) a repo may already have.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from importlib.resources import files
from pathlib import Path

from flywheel_orchestrator._policy import WorkPolicy

# Installed skill names, in pipeline order. Each becomes
# ``.claude/skills/<name>/SKILL.md`` and is invoked as ``/<name>``.
SKILL_NAMES: tuple[str, ...] = (
    "fw-spec",
    "fw-plan",
    "fw-verify",
    "fw-retro",
    "fw-improve",
)

# Sentinel line every generated SKILL.md carries (right after the
# frontmatter). Its presence is the regeneration contract: re-running
# ``flywheel init`` overwrites marker-bearing files with a fresh render;
# deleting the line hands the file to the user permanently.
MANAGED_MARKER: str = "<!-- managed-by: flywheel init -->"

# Default install root, relative to the repo root ``init`` runs in.
DEFAULT_SKILLS_ROOT: Path = Path(".claude/skills")

_TEMPLATE_PACKAGE = "flywheel_orchestrator"
_TEMPLATE_DIR = "_skill_templates"

_DEFAULT_TASKS_DIR = Path(".flywheel/tasks")
_DEFAULT_DB_PATH = Path(".flywheel/flywheel.sqlite")


class SkillTemplateError(RuntimeError):
    """Raised when a template cannot render to a complete skill.

    A leftover ``__FW_*__`` token after substitution means the template
    and the renderer disagree about the placeholder set — surfaced loudly
    so a drifted template can never ship a broken skill.
    """


@dataclass(frozen=True, kw_only=True)
class SkillRenderSettings:
    """The repo-specific values a skill render binds to.

    Derived from the effective work policy (see
    :func:`settings_from_policy`); defaults mirror ``flywheel init``'s
    own defaults so a policy-less repo renders identically to a
    default-answers init. ``state_root`` anchors the conventional
    sibling directories (``specs/``, ``audits/``, ``proposals/``):
    the tasks directory's parent when the directory layout is in use,
    else the store directory.
    """

    tasks_dir: Path = _DEFAULT_TASKS_DIR
    db_path: Path = _DEFAULT_DB_PATH
    source_kind: str = "directory"
    github_repo: str | None = None
    github_label: str | None = None

    @property
    def state_root(self) -> Path:
        if self.source_kind == "directory":
            return self.tasks_dir.parent
        return self.db_path.parent

    @property
    def specs_dir(self) -> Path:
        return self.state_root / "specs"

    @property
    def audits_dir(self) -> Path:
        return self.state_root / "audits"

    @property
    def proposals_dir(self) -> Path:
        return self.state_root / "proposals"

    @property
    def logs_dir(self) -> Path:
        # Mirrors the harness's telemetry root: ``<db dir>/logs``.
        return self.db_path.parent / "logs"


def settings_from_policy(policy: WorkPolicy | None) -> SkillRenderSettings:
    """Bind render settings to the effective policy (or the defaults)."""
    if policy is None:
        return SkillRenderSettings()
    return SkillRenderSettings(
        tasks_dir=policy.tasks_dir or _DEFAULT_TASKS_DIR,
        db_path=policy.db_path or _DEFAULT_DB_PATH,
        source_kind=policy.source_kind,
        github_repo=policy.github_repo,
        github_label=policy.github_label,
    )


@dataclass(frozen=True, kw_only=True)
class SkillInstallReport:
    """Outcome of one :func:`install_skills` sweep, for operator output.

    Paths are the written/skipped ``SKILL.md`` files. ``created`` did not
    exist before; ``updated`` carried the managed marker and was
    regenerated; ``skipped`` exists without the marker (user-owned) and
    was left byte-identical.
    """

    created: tuple[Path, ...] = field(default=())
    updated: tuple[Path, ...] = field(default=())
    skipped: tuple[Path, ...] = field(default=())


def _read_template(name: str) -> str:
    resource = files(_TEMPLATE_PACKAGE) / _TEMPLATE_DIR / f"{name}.md"
    return resource.read_text(encoding="utf-8")


def _delivery_block(settings: SkillRenderSettings) -> str:
    """The ``fw-plan`` delivery section for the configured work source."""
    if (
        settings.source_kind == "github"
        and settings.github_repo
        and settings.github_label
    ):
        block = _read_template("fw-plan-delivery-github")
        block = block.replace("__FW_GH_REPO__", settings.github_repo)
        block = block.replace("__FW_GH_LABEL__", settings.github_label)
        return block.rstrip("\n")
    return _read_template("fw-plan-delivery-directory").rstrip("\n")


def render_skill(name: str, settings: SkillRenderSettings) -> str:
    """Render one skill's ``SKILL.md`` content for ``settings``.

    Raises :class:`SkillTemplateError` for an unknown name or a leftover
    placeholder token after substitution.
    """
    if name not in SKILL_NAMES:
        raise SkillTemplateError(f"unknown skill template {name!r}")
    text = _read_template(name)
    if "__FW_DELIVERY__" in text:
        text = text.replace("__FW_DELIVERY__", _delivery_block(settings))
    replacements = {
        "__FW_TASKS_DIR__": str(settings.tasks_dir),
        "__FW_SPECS_DIR__": str(settings.specs_dir),
        "__FW_AUDITS_DIR__": str(settings.audits_dir),
        "__FW_PROPOSALS_DIR__": str(settings.proposals_dir),
        "__FW_LOGS_DIR__": str(settings.logs_dir),
    }
    for token, value in replacements.items():
        text = text.replace(token, value)
    if "__FW_" in text:
        leftover = text[text.index("__FW_") :].split("__", 2)
        raise SkillTemplateError(
            f"skill template {name!r} rendered with an unbound "
            f"placeholder near {'__'.join(leftover[:2])!r}__"
        )
    return text


def install_skills(
    dest_root: Path,
    settings: SkillRenderSettings,
    *,
    names: tuple[str, ...] = SKILL_NAMES,
) -> SkillInstallReport:
    """Write (or regenerate) the managed skills under ``dest_root``.

    One sweep serves both the fresh-install and the reconfigure path:
    a missing ``SKILL.md`` is created, a marker-bearing one is
    regenerated for the (possibly changed) settings, and a file without
    the marker is user-owned and skipped untouched.
    """
    created: list[Path] = []
    updated: list[Path] = []
    skipped: list[Path] = []
    for name in names:
        target = dest_root / name / "SKILL.md"
        rendered = render_skill(name, settings)
        if not target.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(rendered, encoding="utf-8")
            created.append(target)
            continue
        existing = target.read_text(encoding="utf-8")
        if MANAGED_MARKER not in existing:
            skipped.append(target)
            continue
        if existing != rendered:
            target.write_text(rendered, encoding="utf-8")
        updated.append(target)
    return SkillInstallReport(
        created=tuple(created),
        updated=tuple(updated),
        skipped=tuple(skipped),
    )


__all__ = [
    "DEFAULT_SKILLS_ROOT",
    "MANAGED_MARKER",
    "SKILL_NAMES",
    "SkillInstallReport",
    "SkillRenderSettings",
    "SkillTemplateError",
    "install_skills",
    "render_skill",
    "settings_from_policy",
]

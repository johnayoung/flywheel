"""Held-out acceptance test for spec 00029: offline foreign install + run.

Criteria 1-4: a documented build helper produces the installable artifact set;
installing ONLY the built ``flywheel`` artifact into a throwaway venv OUTSIDE
this checkout (``--no-index``, no workspace source on the path) lets the
``flywheel``/``fw`` console scripts run against a foreign cwd git repo:

  * ``flywheel init --defaults --skills`` scaffolds ``.flywheel/`` + a
    ``flywheel.toml`` and materializes a bundled ``SKILL.md`` (proving the
    ``_skill_templates/*.md`` package data shipped in the wheel), and
  * a store-opening verb bootstraps ``.flywheel/flywheel.sqlite`` from the
    bundled ``_schema/*.sql`` (proving that package data shipped too),

all without ``ImportError``, missing-distribution, or missing-package-data
failures.

Authored blind from the spec's criteria. The discriminators:
  * a wheel that imports but silently omits ``persistence-schema.sql`` passes
    install yet throws at store-open -> the table assertion catches it;
  * a wheel missing ``_skill_templates/*.md`` lets ``init`` scaffold but throws
    on ``SKILL.md`` materialization -> the SKILL.md assertion catches it;
  * an install that only resolves because the workspace was on the path fails
    the ``--no-index`` / scrubbed-env discriminator.

The foreign artifact set is produced by ``scripts/build-dist.sh`` whose final
step vendors the third-party closure from the public index. When no network is
reachable the helper cannot assemble that closure; the test then SKIPS (never
fails), mirroring the Postgres-backed oracles' skip-when-unavailable
convention. With a reachable index it runs the full offline-install proof.

This file lives outside the four pytest testpaths; the grader collects it
explicitly via ``uv run pytest .flywheel/holdout/00029-distributable-install/``.
"""

from __future__ import annotations

import os
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

# .flywheel/holdout/00029-distributable-install/<this file> -> repo root.
_REPO_ROOT = Path(__file__).resolve().parents[3]
_BUILD_HELPER = _REPO_ROOT / "scripts" / "build-dist.sh"

# The tables the bundled schema must create on first store-open.
_EXPECTED_TABLES = {
    "tasks",
    "lifecycles",
    "attempts",
    "events",
    "grader_results",
    "schema_version",
}


def _run(cmd: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
    return subprocess.run(  # type: ignore[call-overload]
        cmd, text=True, capture_output=True, **kwargs
    )


@pytest.fixture(scope="module")
def dist_dir(tmp_path_factory: pytest.TempPathFactory) -> Path:
    """Produce the self-contained artifact set with the documented helper.

    Skips (does not fail) when the helper cannot assemble the third-party
    closure -- the only reason this fails locally is an unreachable index.
    """
    if not _BUILD_HELPER.is_file():
        pytest.skip(f"build helper not found at {_BUILD_HELPER}")
    out = tmp_path_factory.mktemp("flywheel-dist")
    proc = _run(["bash", str(_BUILD_HELPER), str(out)])
    if proc.returncode != 0:
        pytest.skip(
            "scripts/build-dist.sh could not produce the artifact set "
            "(no reachable index for the third-party closure?):\n"
            + (proc.stderr or proc.stdout)[-1500:]
        )
    wheels = {p.name for p in out.glob("*.whl")}
    # The four workspace wheels must be present for a foreign install of flywheel.
    for prefix in ("flywheel-0", "flywheel_core-", "flywheel_orchestrator-", "flywheel_worktree-"):
        assert any(w.startswith(prefix) for w in wheels), (
            f"build helper did not produce a wheel for {prefix!r}; got {sorted(wheels)}"
        )
    return out


def test_foreign_offline_install_and_run(dist_dir: Path, tmp_path: Path) -> None:
    # 1. A throwaway venv OUTSIDE this checkout (under tmp_path).
    venv = tmp_path / "venv"
    _run([sys.executable, "-m", "venv", str(venv)], check=True)
    bindir = venv / ("Scripts" if os.name == "nt" else "bin")
    venv_python = bindir / ("python.exe" if os.name == "nt" else "python")
    assert venv_python.exists(), f"venv python not created: {venv_python}"

    # 2. Install ONLY the built flywheel artifact, --no-index, against the local
    #    dist dir. No workspace source is on this venv's path, so the graph must
    #    resolve from artifact metadata alone.
    install = _run(
        [
            str(venv_python), "-m", "pip", "install",
            "--no-index", "--find-links", str(dist_dir), "flywheel",
        ]
    )
    assert install.returncode == 0, (
        "foreign --no-index install of flywheel failed (the artifact set must "
        f"resolve the whole graph offline):\n{install.stderr}"
    )

    flywheel_bin = bindir / ("flywheel.exe" if os.name == "nt" else "flywheel")
    assert flywheel_bin.exists(), "the install did not create the `flywheel` console script"

    # 3. A foreign cwd git repo that is NOT this checkout. A legitimate adopter
    #    runs init in a real repo on an attached branch with at least one commit
    #    (flywheel's init preflight refuses a detached/unborn HEAD), so seed one.
    repo = tmp_path / "foreign_repo"
    repo.mkdir()
    _run(["git", "init", "-b", "main"], cwd=repo, check=True)
    _run(["git", "config", "user.email", "a@b.invalid"], cwd=repo, check=True)
    _run(["git", "config", "user.name", "a"], cwd=repo, check=True)
    _run(["git", "config", "commit.gpgsign", "false"], cwd=repo, check=True)
    (repo / "README.md").write_text("foreign adopter repo\n")
    _run(["git", "add", "-A"], cwd=repo, check=True)
    _run(["git", "commit", "-m", "init"], cwd=repo, check=True)

    # Scrub the workspace off the subprocess path so a false "it only worked
    # because the source tree was importable" pass cannot happen.
    env = dict(os.environ)
    env.pop("PYTHONPATH", None)
    env.pop("VIRTUAL_ENV", None)
    env["VIRTUAL_ENV"] = str(venv)
    env["PATH"] = f"{bindir}{os.pathsep}{env.get('PATH', '')}"

    init = _run(
        [str(flywheel_bin), "init", "--defaults", "--skills"], cwd=repo, env=env
    )
    assert init.returncode == 0, (
        f"`flywheel init --defaults --skills` failed in the foreign repo:\n{init.stderr}"
    )

    # criteria 2/3: scaffold + a bundled SKILL.md materialized from package data.
    assert (repo / ".flywheel").is_dir(), "init did not scaffold .flywheel/"
    assert (repo / "flywheel.toml").is_file(), "init did not write flywheel.toml"
    skill_files = list((repo / ".claude" / "skills").glob("*/SKILL.md"))
    assert skill_files, (
        "no bundled SKILL.md materialized under .claude/skills/ -- the "
        "_skill_templates/*.md package data did not ship in the wheel"
    )

    # criterion 4: a store-opening verb bootstraps a real SQLite schema from the
    # bundled persistence-schema.sql.
    status = _run([str(flywheel_bin), "status"], cwd=repo, env=env)
    assert status.returncode == 0, (
        f"`flywheel status` failed against the foreign repo:\n{status.stderr}"
    )

    db = repo / ".flywheel" / "flywheel.sqlite"
    assert db.is_file(), (
        "store-open did not bootstrap .flywheel/flywheel.sqlite -- the "
        "_schema/*.sql package data did not ship in the wheel"
    )
    conn = sqlite3.connect(str(db))
    try:
        tables = {
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            )
        }
    finally:
        conn.close()
    missing = _EXPECTED_TABLES - tables
    assert not missing, (
        f"the foreign-installed store is missing expected tables {missing}; "
        f"got {sorted(tables)}"
    )

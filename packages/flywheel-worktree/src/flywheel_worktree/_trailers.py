"""Commit provenance trailers (spec 00078).

The shared vocabulary and mechanism for stamping harness-authoritative git
trailers onto the commits a landing introduces into the base. Consumed by the
merge submit strategy (:class:`~flywheel_worktree.worker.GitWorktreeSubmitter`),
the pr strategy, and the ``fw show`` commit lookup -- the trailer key names and
the value derivation live here so those surfaces agree on one vocabulary.

Stamping is message-only: :func:`stamp_commit_messages` recreates every commit
in ``base..branch`` with an *identical tree object* and *identical
author/committer identity*, changing only the message to carry the
authoritative trailers. The agent is untrusted (``docs/vision.md``): any
agent-authored ``Flywheel-*`` trailer is stripped and replaced with the
harness-derived value (spec 00078, D-2), so a forged provenance value cannot
survive on the landed commit.
"""

from __future__ import annotations

import os
import re
import subprocess
from collections.abc import Mapping, Sequence
from pathlib import Path

#: The three provenance trailer keys, in stamp order. ``fw show`` reads
#: :data:`TRAILER_KEY_RUN` back off a landed commit to resolve its run.
TRAILER_KEY_TASK = "Flywheel-Task"
TRAILER_KEY_RUN = "Flywheel-Run"
TRAILER_KEY_PHASE = "Flywheel-Phase"

#: All provenance keys, in the order they are stamped onto a commit.
PROVENANCE_TRAILER_KEYS: tuple[str, str, str] = (
    TRAILER_KEY_TASK,
    TRAILER_KEY_RUN,
    TRAILER_KEY_PHASE,
)

# Any trailer whose token is ``Flywheel-<word>`` is harness-owned provenance:
# stamping strips every one the agent may have authored before writing the
# authoritative set, so a forged value (``Flywheel-Run: forged-123``) cannot
# survive on the landed commit (spec 00078, D-2). Anchored at line start
# (after optional leading whitespace) and case-insensitive, since git trailer
# tokens match case-blind.
_FLYWHEEL_TRAILER_LINE = re.compile(r"(?i)^\s*Flywheel-[A-Za-z0-9-]+\s*:")

# NUL is not a legal byte in a git ident name/email, so it is a safe field
# separator for reading a commit's author/committer identity in one call. The
# format string carries git's ``%x00`` token (git emits a real NUL there); the
# argument itself holds no NUL, since an embedded NUL is illegal in argv. The
# output is split on :data:`_FIELD_SEP`, the actual NUL git wrote.
_FIELD_SEP = "\x00"
_IDENTITY_FORMAT = "%x00".join(
    ("%an", "%ae", "%aI", "%cn", "%ce", "%cI")
)


def provenance_trailers(
    *, task_id: str, run_id: str, phase: str
) -> tuple[tuple[str, str], ...]:
    """The authoritative ``(key, value)`` trailers for a landed commit.

    The value derivation every stamping surface shares: the landing task's id,
    its run id, and the phase directory name the task lives under. Ordered to
    match :data:`PROVENANCE_TRAILER_KEYS`; the pr strategy and ``fw show``
    consume this same derivation so the record is uniform across land paths.
    """
    return (
        (TRAILER_KEY_TASK, task_id),
        (TRAILER_KEY_RUN, run_id),
        (TRAILER_KEY_PHASE, phase),
    )


def stamp_commit_messages(
    repo: Path,
    *,
    base: str,
    branch: str,
    trailers: Sequence[tuple[str, str]],
) -> str:
    """Rewrite every commit in ``base..branch`` to carry ``trailers``.

    Message-only: each commit is recreated with :manpage:`git-commit-tree(1)`
    over the *original commit's exact tree object* and its original
    author/committer identity, so the ordered tree list of the rewritten range
    is byte-identical to the original (spec 00078, criterion 4). Any
    agent-authored ``Flywheel-*`` trailer is stripped before the authoritative
    trailers are appended (D-2), and each key ends up exactly once.

    Returns the rewritten branch-tip sha (equal to the current tip when the
    range is empty). Does not move any ref -- the caller advances the branch and
    worktree onto the returned sha. Runs against ``repo``'s shared object DB, so
    the new objects are visible from every linked worktree. Raises
    :class:`subprocess.CalledProcessError` on any git failure; the submit path
    swallows it into a park (the land is suppressed, never a broken land).
    """
    revs = _run(
        repo, "rev-list", "--topo-order", "--reverse", f"{base}..{branch}"
    ).split()
    old_tip = _run(repo, "rev-parse", branch)
    if not revs:
        return old_tip
    remap: dict[str, str] = {}
    for sha in revs:
        # ``rev-list --parents -n 1`` prints ``<sha> <parent>...``; drop the
        # commit itself. Parents inside the rewritten range are remapped to
        # their new shas; the base (and any parent outside the range) is left
        # as-is, so ``base`` stays the root ancestor and the fast-forward holds.
        parents = _run(repo, "rev-list", "--parents", "-n", "1", sha).split()[1:]
        tree = _run(repo, "rev-parse", f"{sha}^{{tree}}")
        message = _stamped_message(repo, sha, trailers)
        args = ["commit-tree", tree]
        for parent in parents:
            args += ["-p", remap.get(parent, parent)]
        remap[sha] = _run(
            repo, *args, input_text=message, env=_identity_env(repo, sha)
        )
    return remap[old_tip]


def _stamped_message(
    repo: Path, sha: str, trailers: Sequence[tuple[str, str]]
) -> str:
    """The commit's message with agent ``Flywheel-*`` trailers stripped and the
    authoritative ``trailers`` appended as a well-formed trailer block."""
    raw = _run_raw(repo, "log", "-1", "--format=%B", sha)
    stripped = "\n".join(
        line
        for line in raw.splitlines()
        if not _FLYWHEEL_TRAILER_LINE.match(line)
    )
    args = [
        "interpret-trailers",
        "--if-exists",
        "replace",
        "--if-missing",
        "add",
    ]
    for key, value in trailers:
        args += ["--trailer", f"{key}: {value}"]
    return _run_raw(repo, *args, input_text=stripped)


def _identity_env(repo: Path, sha: str) -> dict[str, str]:
    """The environment that reproduces ``sha``'s author and committer identity
    (name, email, and date, timezone included) for :manpage:`git-commit-tree`,
    layered over the current process env so git still resolves ``PATH`` etc."""
    raw = _run(repo, "log", "-1", f"--format={_IDENTITY_FORMAT}", sha)
    author_name, author_email, author_date, committer = raw.split(_FIELD_SEP, 3)
    committer_name, committer_email, committer_date = committer.split(_FIELD_SEP)
    env = dict(os.environ)
    env.update(
        GIT_AUTHOR_NAME=author_name,
        GIT_AUTHOR_EMAIL=author_email,
        GIT_AUTHOR_DATE=author_date,
        GIT_COMMITTER_NAME=committer_name,
        GIT_COMMITTER_EMAIL=committer_email,
        GIT_COMMITTER_DATE=committer_date,
    )
    return env


def _run(
    repo: Path,
    *args: str,
    input_text: str | None = None,
    env: Mapping[str, str] | None = None,
) -> str:
    """Run a git command and return its stdout stripped of trailing whitespace
    (the common case: a sha, a field list, a rev range)."""
    return _run_raw(repo, *args, input_text=input_text, env=env).strip()


def _run_raw(
    repo: Path,
    *args: str,
    input_text: str | None = None,
    env: Mapping[str, str] | None = None,
) -> str:
    """Run ``git -C <repo> <args>`` and return raw stdout. ``check=True``: a
    failure here is unexpected (the caller holds the merge lock and has already
    validated the branch), so it raises rather than silently mis-stamping."""
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        text=True,
        capture_output=True,
        check=True,
        input=input_text,
        env=dict(env) if env is not None else None,
    ).stdout

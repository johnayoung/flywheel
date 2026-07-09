"""Cross-task surface-overlap lint (authoring-time gate).

Two active tasks that write the *same* file surface with no declared
coordination are a data-loss trap: they can be dispatched concurrently, both
edit the shared file, and the loser's rebase collides -- the infrared incident
of 2026-07-09 (``wsteth-goldens`` and ``balancer-bpt-base-golden`` carried
disjoint conflict-key sets, both wrote the shared golden-harness surface, ran
concurrently, and the loser's rebase collision triggered the P5 data loss).

This module is the static, pre-dispatch detector for that shape. It is pairwise
over the active listing only: it never executes a grader, never reads the store,
and never checks whether a derived path exists on disk (an output path a task
will create is still a surface -- the same rationale that tabled
``flywheel_core.validation``'s missing-path check).

A task's **surface** is the union of the file paths it declares it touches:

* ``context.relevant`` entries, each with a trailing `` -- comment`` annotation
  stripped (no other code parses that convention -- it is implemented here);
* path tokens pulled from every ``command`` grader's ``run`` string, with
  ``pytest`` ``::node-ids`` reduced to the file path.

Every candidate passes the one true normalization autopilot uses
(:func:`_conflict_key_for_path`): empty, absolute, parent-escaping,
directory-like, and manifest/lockfile paths drop out, and any candidate
containing ``://`` (a URL) contributes nothing.

A pair is **flagged** when their surfaces intersect AND their ``conflict_keys``
sets are disjoint (a shared key already serializes them at claim time) AND at
least one overlapping path is listed in neither task's top-level ``overlap_ok``
allow-list AND neither task is reachable from the other over the active
listing's ``prerequisites`` edges (transitively -- a chained pair already runs
in sequence). ``overlap_ok`` is a validate-time-only orchestration key; this
lint is its only consumer.
"""

from __future__ import annotations

import shlex
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from flywheel_core.task import CommandGrader, Task
from flywheel_core.validation import TaskDefect

from flywheel_orchestrator._sources import (
    _read_overlap_ok,
    _read_prerequisites,
    _read_scheduling_metadata,
)

#: Build-manifest / lockfile basenames that make poor conflict resources: many
#: legitimately-distinct tasks land against the same manifest, so treating it as
#: a shared surface would flag unrelated work. The one true list, shared with
#: autopilot's claim-key derivation (which re-imports it from here).
_MANIFEST_BASENAMES: frozenset[str] = frozenset(
    {
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
        "go.mod",
        "go.sum",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
    }
)


def _conflict_key_for_path(raw: str) -> str | None:
    """Normalize a repo-relative path into a stable conflict-key token, or
    ``None`` when it is too coarse to be a useful contended resource.

    Returns ``None`` for a path that is empty, absolute, parent-escaping,
    directory-like (no file suffix -- e.g. ``crates/``), or a shared build
    manifest / lockfile (see :data:`_MANIFEST_BASENAMES`). A specific source
    file (``crates/infrared-feed/src/tycho.rs``) returns its normalized posix
    form so two tasks scoped to it are seen as the same surface. This is the
    single normalization autopilot's claim keys and this lint's surfaces both
    pass through.
    """
    s = raw.strip()
    if not s:
        return None
    p = PurePosixPath(s)
    if p.is_absolute() or any(part == ".." for part in p.parts):
        return None
    if p.suffix == "":  # directory-like (or extensionless): too coarse
        return None
    if p.name in _MANIFEST_BASENAMES:
        return None
    return p.as_posix()


@dataclass(frozen=True, kw_only=True)
class TaskSurface:
    """One active task's derived file surface plus its coordination metadata.

    ``paths`` is the normalized surface (see module docstring). ``conflict_keys``
    (a shared key exempts the pair -- claim-time serialization already covers
    it), ``overlap_ok`` (per-path allow-list, exempts only the listed path), and
    ``prerequisites`` (a chain exempts the pair) are the three coordination
    signals the pairwise check consults.
    """

    task_id: str
    paths: frozenset[str]
    conflict_keys: frozenset[str] = frozenset()
    overlap_ok: frozenset[str] = frozenset()
    prerequisites: tuple[str, ...] = ()


def _strip_relevant_annotation(entry: str) -> str:
    """Return a ``context.relevant`` entry with any `` -- comment`` suffix cut.

    The `` -- `` convention (path, then a free-text note) is documented but
    parsed nowhere else, so the strip is implemented here. Only the first
    occurrence splits, so a note that itself contains `` -- `` is discarded
    whole.
    """
    marker = " -- "
    idx = entry.find(marker)
    return (entry[:idx] if idx != -1 else entry).strip()


def _run_path_tokens(run: str) -> list[str]:
    """Path-candidate tokens from a command grader's ``run`` string.

    Tokenized as a shell command (falling back to whitespace split when the
    string does not parse -- an unparseable ``run`` is a separate defect the
    grader check already surfaces). Each token has any ``pytest`` ``::node-id``
    suffix reduced to the leading file path; the URL and normalization filters
    are applied downstream by :func:`task_surface`.
    """
    try:
        tokens = shlex.split(run)
    except ValueError:
        tokens = run.split()
    return [token.split("::", 1)[0] for token in tokens]


def task_surface(task: Task) -> frozenset[str]:
    """Derive ``task``'s normalized file surface (empty when it declares none).

    The union of ``context.relevant`` entries (annotation-stripped) and every
    ``command`` grader ``run``'s path tokens, each passed through the URL filter
    and :func:`_conflict_key_for_path`. Never touches the filesystem -- an
    output path a task will create is still a surface.
    """
    surface: set[str] = set()
    for entry in task.context.relevant:
        _add_candidate(surface, _strip_relevant_annotation(entry))
    for grader in task.graders:
        if isinstance(grader, CommandGrader):
            for token in _run_path_tokens(grader.run):
                _add_candidate(surface, token)
    return frozenset(surface)


def _add_candidate(surface: set[str], candidate: str) -> None:
    """Add ``candidate`` to ``surface`` when it normalizes to a real path.

    URLs (anything containing ``://``) contribute nothing; everything else is
    run through :func:`_conflict_key_for_path`, which drops the coarse and
    unusable forms.
    """
    if "://" in candidate:
        return
    key = _conflict_key_for_path(candidate)
    if key is not None:
        surface.add(key)


def build_surface(path: Path, task: Task) -> TaskSurface:
    """Assemble a :class:`TaskSurface` from a loaded task and its source file.

    The surface comes from the compiled :class:`~flywheel_core.task.Task`; the
    three coordination signals (``conflict_keys``, ``overlap_ok``,
    ``prerequisites``) are top-level orchestration keys core ``flywheel``
    ignores, so they are read straight from the task file with the tolerant
    ``_sources`` readers (an absent or malformed key yields its default).
    """
    _priority, _capabilities, conflict_keys = _read_scheduling_metadata(path)
    return TaskSurface(
        task_id=task.id,
        paths=task_surface(task),
        conflict_keys=conflict_keys,
        overlap_ok=frozenset(_read_overlap_ok(path)),
        prerequisites=_read_prerequisites(path),
    )


def _reachability(surfaces: Sequence[TaskSurface]) -> dict[str, frozenset[str]]:
    """Transitive prerequisite reachability, over active-listing edges only.

    For each task, the set of task ids reachable by following ``prerequisites``
    edges. An edge whose target is not in the active listing is dropped, so a
    path through an archived (absent) task does not connect the two ends -- the
    exemption is intentionally not inherited across a gap.
    """
    present = {s.task_id for s in surfaces}
    adjacency: dict[str, tuple[str, ...]] = {
        s.task_id: tuple(p for p in s.prerequisites if p in present)
        for s in surfaces
    }
    closure: dict[str, frozenset[str]] = {}
    for start in adjacency:
        seen: set[str] = set()
        stack = list(adjacency.get(start, ()))
        while stack:
            node = stack.pop()
            if node in seen:
                continue
            seen.add(node)
            stack.extend(adjacency.get(node, ()))
        closure[start] = frozenset(seen)
    return closure


def _overlap_detail(task_a: str, task_b: str, paths: Sequence[str]) -> str:
    """The defect detail: both task ids, every overlapping path, remediations."""
    joined = ", ".join(paths)
    return (
        f"surface overlap between tasks {task_a!r} and {task_b!r} on: "
        f"{joined}. Resolve by declaring a shared conflict_keys entry, "
        f"listing each path in one task's overlap_ok, or chaining the tasks "
        f"via prerequisites."
    )


def surface_overlap_defects(
    surfaces: Sequence[TaskSurface],
) -> list[TaskDefect]:
    """Pairwise surface-overlap defects across the active listing.

    Returns one :class:`~flywheel_core.validation.TaskDefect` per flagged pair
    (attached to the first task of the pair in listing order). A pair is flagged
    when their surfaces intersect, their ``conflict_keys`` are disjoint, at
    least one overlapping path is in neither task's ``overlap_ok``, and neither
    is reachable from the other over prerequisites. Empty when nothing overlaps.
    """
    reachable = _reachability(surfaces)
    defects: list[TaskDefect] = []
    for i in range(len(surfaces)):
        for j in range(i + 1, len(surfaces)):
            a = surfaces[i]
            b = surfaces[j]
            overlap = a.paths & b.paths
            if not overlap:
                continue
            if a.conflict_keys & b.conflict_keys:
                continue  # a shared key already serializes them at claim time
            allowed = a.overlap_ok | b.overlap_ok
            flagged = sorted(p for p in overlap if p not in allowed)
            if not flagged:
                continue  # every overlapping path is individually allow-listed
            if (
                b.task_id in reachable.get(a.task_id, frozenset())
                or a.task_id in reachable.get(b.task_id, frozenset())
            ):
                continue  # a prerequisite chain already runs them in sequence
            defects.append(
                TaskDefect(
                    task_id=a.task_id,
                    detail=_overlap_detail(a.task_id, b.task_id, flagged),
                )
            )
    return defects

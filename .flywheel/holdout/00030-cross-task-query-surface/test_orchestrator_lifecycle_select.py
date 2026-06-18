"""Held-out acceptance test (blind to implementation) -- SOURCE-SCAN oracle.

Criterion: the cross-task lifecycle reads in the orchestrator no longer reach
past the store protocol into the private connection. After the 00030 migration
the lifecycle-listing/filtering ``store._connection.execute(...)`` sites (and
their ``# noqa: SLF001`` markers) that perform a lifecycle ``SELECT`` are gone,
replaced by calls to the public ``list_lifecycles(...)`` method.

This test does NOT import or run the migrated code. It reads the orchestrator's
two source files as TEXT and asserts that no ``store._connection``-driven SQL
``SELECT ... FROM lifecycles`` remains in either file.

Defends against: adding the public protocol method but leaving the raw-SQL call
sites in place (a no-op that passes the other 00030 criteria in isolation).

Scope (decision D-4): per-run ``store._connection`` reads of the ``attempts``
table (token/cost rollups and the per-run attempts join) STAY on the private
connection. The scan is therefore table-scoped to lifecycle SELECTs and must
NOT flag an ``attempts``-table read. We deliberately do NOT assert the bare
absence of ``_connection`` / ``SLF001`` -- that would false-positive on those
legitimate attempts reads.
"""

from __future__ import annotations

import re
from pathlib import Path

# Relative-to-repo-root paths of the two orchestrator source files to scan.
_REL_PATHS = (
    "packages/flywheel-orchestrator/src/flywheel_orchestrator/_history.py",
    "packages/flywheel-orchestrator/src/flywheel_orchestrator/_workflow.py",
)


def _repo_root() -> Path:
    """Walk parents of this test file until we find the dir holding ``packages/``.

    Robust to where the holdout tree lives relative to the repo root; we never
    hardcode an absolute machine path.
    """
    for parent in Path(__file__).resolve().parents:
        if (parent / "packages").is_dir():
            return parent
    raise AssertionError(
        "could not locate repo root (no parent of "
        f"{__file__!r} contains a 'packages/' directory)"
    )


def _read_source(rel_path: str) -> str:
    path = _repo_root() / rel_path
    assert path.is_file(), f"expected source file to exist: {path}"
    with open(path, "r", encoding="utf-8") as fh:  # test-side I/O only
        return fh.read()


# --- table-scoped lifecycle-SELECT detection ---------------------------------
#
# A violation is a SQL ``SELECT ... FROM lifecycles`` that is executed against
# the store's PRIVATE connection. The private connection is reached either
# directly (``store._connection.execute(...)``) or through a local alias bound
# to it (``conn = store._connection`` then ``conn.execute(...)``). We must catch
# both spellings while leaving ``FROM attempts`` reads -- which legitimately stay
# on the private connection per D-4 -- untouched.

# Matches ``<name> = store._connection`` (alias binding the private connection),
# e.g. ``conn = store._connection`` (carrying an SLF001 suppression).
# ``store._connection`` itself is always a private-connection caller and is
# added explicitly below.
_ALIAS_BIND = re.compile(r"^\s*([A-Za-z_]\w*)\s*=\s*store\._connection\b")

# Matches a ``.execute(`` call whose receiver is a given caller name.
# We build one of these per known private-connection caller name.
def _execute_call_re(caller: str) -> re.Pattern[str]:
    return re.compile(rf"\b{re.escape(caller)}\s*\.\s*execute\s*\(")


def _strip_line_comment(line: str) -> str:
    """Drop a Python ``#`` line comment tail (commented-out code is not live).

    A lifecycle SELECT that has merely been commented out is treated as REMOVED:
    a ``# ...`` line is no longer an executable private-connection read, which is
    exactly the behavior the criterion guards. We do not attempt to honor ``#``
    inside string literals here -- the orchestrator SQL blocks do not contain a
    bare ``#``, so a conservative split is safe for these files.
    """
    return line.split("#", 1)[0]


def _strip_sql_comment(text: str) -> str:
    """Drop SQL ``--`` comment tails line-by-line so commented SQL is inert."""
    out: list[str] = []
    for line in text.splitlines():
        out.append(line.split("--", 1)[0])
    return "\n".join(out)


_FROM_LIFECYCLES = re.compile(r"\bfrom\s+lifecycles\b", re.IGNORECASE)
_SELECT = re.compile(r"\bselect\b", re.IGNORECASE)


def _private_connection_caller_names(code_lines: list[str]) -> set[str]:
    """Names that resolve to the private connection in this file.

    Always includes the literal ``store._connection`` receiver, plus any local
    variable bound via ``<name> = store._connection`` (e.g. ``conn``).
    """
    callers: set[str] = {"store._connection"}
    for line in code_lines:
        code = _strip_line_comment(line)
        m = _ALIAS_BIND.match(code)
        if m is not None:
            callers.add(m.group(1))
    return callers


def _find_lifecycle_select_violations(source: str) -> list[int]:
    """Return 0-based line indices of private-connection lifecycle SELECTs.

    For every ``<private-caller>.execute(`` call found on a non-comment code
    line, we read the SQL argument that follows (the contiguous text up to the
    matching ``execute(...)`` close, conservatively the next ``)`` that ends the
    call) and flag it iff, after stripping comments, it is a ``SELECT`` whose
    ``FROM`` target is the ``lifecycles`` table.
    """
    raw_lines = source.splitlines()
    callers = _private_connection_caller_names(raw_lines)
    execute_res = [_execute_call_re(c) for c in callers]

    violations: list[int] = []
    for idx, line in enumerate(raw_lines):
        code = _strip_line_comment(line)
        if not any(rx.search(code) for rx in execute_res):
            continue
        # This line opens a private-connection ``.execute(`` call. Capture the
        # SQL: the call argument almost always begins with a triple-quoted
        # string on the following lines. We scan forward, accumulating text
        # until the call is closed by a line whose stripped form ends the call
        # (a line ending in ``)`` at the call's outer level). To stay robust and
        # avoid over-reading into the next statement, we bound the window at the
        # next line that itself opens a new ``.execute(`` call or that is clearly
        # past the SQL (a ``).fetch`` / ``)`` terminator).
        sql_chunk: list[str] = [code]
        j = idx + 1
        while j < len(raw_lines):
            nxt = raw_lines[j]
            sql_chunk.append(nxt)
            stripped = nxt.strip()
            # Terminate at the line that closes this execute call. The repo
            # spells the close as ``)`` , ``).fetchone()`` , ``).fetchall()`` ,
            # or ``"""`` followed by args then ``)`` -- detecting the closing
            # paren of the call at column-leading position is sufficient here.
            if stripped.startswith(")") or stripped.endswith(").fetchone()") \
                    or stripped.endswith(").fetchall()") \
                    or stripped == ")":
                break
            j += 1

        sql_text = _strip_sql_comment("\n".join(sql_chunk))
        if _SELECT.search(sql_text) and _FROM_LIFECYCLES.search(sql_text):
            violations.append(idx)
    return violations


def test_orchestrator_lifecycle_select_off_private_connection() -> None:
    """No ``store._connection`` lifecycle SELECT may remain in either file.

    Fails (with the offending file + 1-based line numbers) if any private-
    connection ``SELECT ... FROM lifecycles`` survives the migration. Passes
    when every lifecycle read has moved onto ``list_lifecycles(...)`` -- while
    leaving the legitimate ``store._connection`` ``FROM attempts`` reads in
    place (those are never flagged because the scan is scoped to the
    ``lifecycles`` table).
    """
    failures: list[str] = []
    for rel_path in _REL_PATHS:
        source = _read_source(rel_path)
        for line_idx in _find_lifecycle_select_violations(source):
            failures.append(f"{rel_path}:{line_idx + 1}")

    assert not failures, (
        "store._connection lifecycle SELECT (SELECT ... FROM lifecycles) still "
        "present -- the cross-task lifecycle read did not move onto the public "
        "list_lifecycles(...) surface. Offending private-connection lifecycle "
        "SELECT site(s):\n  " + "\n  ".join(failures)
    )

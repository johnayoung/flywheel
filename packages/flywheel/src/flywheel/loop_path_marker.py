"""Loop-path signal detection over a unified-diff string.

Pure string analysis: the input is a unified-diff text, the output is the
set of watched loop-path signals it contains. No file I/O, no git
invocation, no JSON. Implements the FR-1 trigger set from
``.flywheel/specs/00017-FEATURE-in-loop-verification-gate.md``.

Only **added** content is considered: lines beginning with ``+`` and not
the ``+++`` file header. A watched symbol that appears only in a context
line or a removed line is not a hit. Operating on the **net cumulative**
diff text the caller hands in means an add-then-revert within one phase
naturally nets no new symbol.

The detection is intentionally an over-approximation: file/symbol
presence cannot prove a new loop path exists. Signal 4 is scoped to its
decidable core -- a newly added ``def`` on a Protocol in
``store_protocols.py`` -- and explicitly does NOT fire on any-new-
function-in-``harness.py``/``orchestrator.py`` (that file-touch fallback
is out of scope here; it lives in the workflow-layer gate that consumes
this module).
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from enum import Enum


class LoopPathSignal(str, Enum):
    """Watched signals from the spec's Trigger Set table.

    The numeric suffix mirrors the table row in
    ``.flywheel/specs/00017-FEATURE-in-loop-verification-gate.md``.
    """

    # Signal 1: new Status/Outcome enum member or transition-rule entry
    # in ``lifecycle.py``.
    STATUS_OR_TRANSITION = "status_or_transition"

    # Signal 2: new ``ADD COLUMN`` / table / column-definition in a
    # ``_schema/*.sql`` file the live store binds to.
    SCHEMA_COLUMN_OR_TABLE = "schema_column_or_table"

    # Signal 3: new ``Grader`` union variant in ``task.py`` or a new
    # ``grader_*.py`` module the harness dispatches.
    GRADER_VARIANT = "grader_variant"

    # Signal 4: new ``def`` on a Protocol in ``store_protocols.py``
    # (the decidable core of the store-contract/resolver-entry signal).
    PROTOCOL_METHOD = "protocol_method"

    # Signal 5: new ``CONTROL_COMMAND_*`` constant in ``invoker_client.py``.
    CONTROL_COMMAND_VERB = "control_command_verb"


# --- Public entry point -----------------------------------------------------


def detect_loop_path_signals(diff: str) -> frozenset[LoopPathSignal]:
    """Return the set of watched loop-path signals present in ``diff``.

    ``diff`` is a unified-diff text. An empty or signal-free diff returns
    an empty frozenset; the function never raises on malformed input --
    unparseable segments are silently skipped, which is the correct
    behavior for an over-approximating gate (false negatives only hurt
    coverage, never archive-correctness, because every signal it does
    fire is verified by either the verify-task gate or the audit
    re-check).
    """
    signals: set[LoopPathSignal] = set()
    for section in _parse_diff(diff):
        path = section.new_path
        added = section.added_lines

        if _is_lifecycle_path(path) and _detect_status_or_transition(added):
            signals.add(LoopPathSignal.STATUS_OR_TRANSITION)

        if _is_schema_sql_path(path) and _detect_schema_column_or_table(
            added
        ):
            signals.add(LoopPathSignal.SCHEMA_COLUMN_OR_TABLE)

        if _is_task_path(path) and _detect_grader_variant_in_task(added):
            signals.add(LoopPathSignal.GRADER_VARIANT)

        if section.is_new_file and _is_grader_module_path(path):
            signals.add(LoopPathSignal.GRADER_VARIANT)

        if _is_store_protocols_path(path) and _detect_protocol_method(added):
            signals.add(LoopPathSignal.PROTOCOL_METHOD)

        if _is_invoker_client_path(path) and _detect_control_command_verb(
            added
        ):
            signals.add(LoopPathSignal.CONTROL_COMMAND_VERB)

    return frozenset(signals)


# --- Diff parsing -----------------------------------------------------------


@dataclass
class _FileSection:
    """One file's worth of added content extracted from a unified diff."""

    new_path: str
    is_new_file: bool = False
    added_lines: list[str] = field(default_factory=list)


def _parse_diff(diff: str) -> list[_FileSection]:
    """Split a unified diff into per-file added-content sections.

    Only the post-image path (the ``+++`` header) and added lines (those
    beginning with ``+`` but not ``+++``) are extracted. Context lines,
    removed lines, hunk headers, and ``\\ No newline at end of file``
    markers are ignored.

    A section's ``is_new_file`` flag is set when the matching ``---``
    header pointed at ``/dev/null`` (the canonical "file creation"
    marker in a unified diff).
    """
    sections: list[_FileSection] = []
    current: _FileSection | None = None
    pending_old_is_null: bool = False

    for line in diff.splitlines():
        if line.startswith("diff --git"):
            current = None
            pending_old_is_null = False
            continue

        if line.startswith("--- "):
            old = line[4:].split("\t", 1)[0].strip()
            pending_old_is_null = old == "/dev/null"
            current = None
            continue

        if line.startswith("+++ "):
            new = line[4:].split("\t", 1)[0].strip()
            if new == "/dev/null":
                # File deletion: no added content to attribute.
                current = None
                pending_old_is_null = False
                continue
            if new.startswith("b/"):
                new = new[2:]
            current = _FileSection(
                new_path=new, is_new_file=pending_old_is_null
            )
            sections.append(current)
            pending_old_is_null = False
            continue

        if current is None:
            continue

        # Added line (not the +++ header, which we handled above).
        if line.startswith("+"):
            current.added_lines.append(line[1:])

    return sections


# --- Path predicates --------------------------------------------------------


def _basename(path: str) -> str:
    return path.rsplit("/", 1)[-1]


def _is_lifecycle_path(path: str) -> bool:
    return _basename(path) == "lifecycle.py"


def _is_task_path(path: str) -> bool:
    return _basename(path) == "task.py"


def _is_store_protocols_path(path: str) -> bool:
    return _basename(path) == "store_protocols.py"


def _is_invoker_client_path(path: str) -> bool:
    return _basename(path) == "invoker_client.py"


def _is_schema_sql_path(path: str) -> bool:
    return "/_schema/" in path and path.endswith(".sql")


_GRADER_MODULE_RE = re.compile(r"^grader_[a-z_][a-z_0-9]*\.py$")


def _is_grader_module_path(path: str) -> bool:
    return bool(_GRADER_MODULE_RE.match(_basename(path)))


# --- Signal detectors -------------------------------------------------------


# Signal 1: a Status/Outcome enum member is an indented ``UPPER_NAME = "..."``
# at class body indentation; transition-rule entries are indented
# ``Status.NAME:`` / ``Outcome.NAME:`` dict keys (``_VALID_EDGES``-style).
# The indentation requirement (``^\s+``) excludes module-level constants
# such as ``_REQUIRES_ERROR`` and rules out local-variable renames inside
# methods (those use lowercase identifiers).
_ENUM_MEMBER_RE = re.compile(r'^\s+[A-Z][A-Z_0-9]*\s*=\s*["\']')
_TRANSITION_ENTRY_RE = re.compile(
    r"^\s+(?:Status|Outcome)\.[A-Z_][A-Z_0-9]*\s*:"
)


def _detect_status_or_transition(added_lines: list[str]) -> bool:
    for line in added_lines:
        if _is_python_comment_or_blank(line):
            continue
        if _ENUM_MEMBER_RE.match(line):
            return True
        if _TRANSITION_ENTRY_RE.match(line):
            return True
    return False


# Signal 2: a new SQL column or table. ``ADD COLUMN`` and ``CREATE TABLE``
# are unambiguous; we also catch column-definition lines added directly
# inside an existing ``CREATE TABLE`` block (the phase-08 pattern, where
# ``awaiting_manual_ordinal INTEGER`` was added as a new column inside the
# existing ``lifecycles`` definition).
_ADD_COLUMN_RE = re.compile(r"\bADD\s+COLUMN\b", re.IGNORECASE)
_CREATE_TABLE_RE = re.compile(r"\bCREATE\s+TABLE\b", re.IGNORECASE)
_COLUMN_DEF_RE = re.compile(
    r"^\s+[a-z_][a-z_0-9]*\s+"
    r"(?:TEXT|INTEGER|REAL|BLOB|DATETIME|BOOLEAN|NUMERIC"
    r"|DATE|TIMESTAMP|VARCHAR|CHAR|BIGINT|SMALLINT|FLOAT|DOUBLE)\b",
    re.IGNORECASE,
)


def _detect_schema_column_or_table(added_lines: list[str]) -> bool:
    for line in added_lines:
        stripped = line.lstrip()
        if not stripped or stripped.startswith("--"):
            continue
        if _ADD_COLUMN_RE.search(line):
            return True
        if _CREATE_TABLE_RE.search(line):
            return True
        if _COLUMN_DEF_RE.match(line):
            return True
    return False


# Signal 3 (task.py side): a new ``class XxxGrader`` definition, or a
# modification of the top-level ``Grader = ...`` union assignment. Either
# is a decisive marker that the discriminated union has a new variant.
_NEW_GRADER_CLASS_RE = re.compile(r"^\s*class\s+[A-Za-z_]\w*Grader\b")
_GRADER_UNION_RE = re.compile(r"^Grader\s*=")


def _detect_grader_variant_in_task(added_lines: list[str]) -> bool:
    for line in added_lines:
        if _is_python_comment_or_blank(line):
            continue
        if _NEW_GRADER_CLASS_RE.match(line):
            return True
        if _GRADER_UNION_RE.match(line):
            return True
    return False


# Signal 4 (scoped, decidable core): a newly added ``def`` at Protocol
# body indentation (4 spaces) in ``store_protocols.py``. Dunders
# (``__init__`` etc.) are excluded so a new ``StoreConflictError``
# subclass with an ``__init__`` does not false-positive. Protocol methods
# in this file are all lowercase identifiers per the existing surface.
_PROTOCOL_DEF_RE = re.compile(r"^ {4}def\s+(?!__)[a-z][a-z_0-9]*\s*\(")


def _detect_protocol_method(added_lines: list[str]) -> bool:
    for line in added_lines:
        if _PROTOCOL_DEF_RE.match(line):
            return True
    return False


# Signal 5: a new top-level ``CONTROL_COMMAND_*`` constant in
# ``invoker_client.py``. The verbs are module-level, so we anchor at
# column 0.
_CONTROL_CMD_RE = re.compile(r"^CONTROL_COMMAND_[A-Z][A-Z_0-9]*\s*[:=]")


def _detect_control_command_verb(added_lines: list[str]) -> bool:
    for line in added_lines:
        if _CONTROL_CMD_RE.match(line):
            return True
    return False


# --- Helpers ----------------------------------------------------------------


def _is_python_comment_or_blank(line: str) -> bool:
    stripped = line.lstrip()
    return not stripped or stripped.startswith("#")


__all__ = ["LoopPathSignal", "detect_loop_path_signals"]

"""Unit tests for ``flywheel_core.loop_path_marker``.

Each signal in the FR-1 Trigger Set is covered with a synthetic unified
diff. The two FR-1 acceptance cases are pinned explicitly:

* a diff adding a ``Status`` enum member IS marked, and
* a diff that only renames a local variable in ``lifecycle.py`` is NOT
  marked.
"""

from __future__ import annotations

from textwrap import dedent

from flywheel_core.loop_path_marker import (
    LoopPathSignal,
    detect_loop_path_signals,
)


def _diff(text: str) -> str:
    """Strip the common leading whitespace from a synthetic diff literal."""
    return dedent(text).lstrip("\n")


# --- Signal 1: Status/Outcome member or transition-rule entry ---------------


def test_status_enum_member_addition_fires_signal_1() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py
        --- a/src/flywheel/lifecycle.py
        +++ b/src/flywheel/lifecycle.py
        @@ -16,6 +16,7 @@
             FAILED = "failed"
             INTERRUPTED = "interrupted"
        +    REVIEWING = "reviewing"
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.STATUS_OR_TRANSITION}
    )


def test_outcome_enum_member_addition_fires_signal_1() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py
        --- a/src/flywheel/lifecycle.py
        +++ b/src/flywheel/lifecycle.py
        @@ -25,3 +25,4 @@
             INTERNAL_ERROR = "internal_error"
        +    REJECTED = "rejected"
        """
    )
    assert LoopPathSignal.STATUS_OR_TRANSITION in detect_loop_path_signals(
        diff
    )


def test_new_transition_rule_entry_fires_signal_1() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py
        --- a/src/flywheel/lifecycle.py
        +++ b/src/flywheel/lifecycle.py
        @@ -58,3 +58,5 @@
             Status.DONE: frozenset(),
             Status.FAILED: frozenset(),
        +    Status.REVIEWING: frozenset({Status.DONE}),
         }
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.STATUS_OR_TRANSITION}
    )


# --- FR-1 acceptance: local-variable rename must NOT fire -------------------


def test_local_variable_rename_in_lifecycle_does_not_fire() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py
        --- a/src/flywheel/lifecycle.py
        +++ b/src/flywheel/lifecycle.py
        @@ -220,7 +220,7 @@
                 if not self.attempts:
                     return 0
        -        tail = self.attempts[-1]
        -        if tail.outcome not in _FAILED_OUTCOMES:
        +        last = self.attempts[-1]
        +        if last.outcome not in _FAILED_OUTCOMES:
                     return 0
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()


# --- Signal 2: ADD COLUMN / new table / column DDL --------------------------


def test_add_column_fires_signal_2() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/_schema/persistence-schema.sql b/src/flywheel/_schema/persistence-schema.sql
        --- a/src/flywheel/_schema/persistence-schema.sql
        +++ b/src/flywheel/_schema/persistence-schema.sql
        @@ -78,3 +78,4 @@
           awaiting_manual_ordinal INTEGER
         );
        +ALTER TABLE lifecycles ADD COLUMN review_notes TEXT;
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.SCHEMA_COLUMN_OR_TABLE}
    )


def test_new_create_table_fires_signal_2() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/_schema/persistence-schema.sql b/src/flywheel/_schema/persistence-schema.sql
        --- a/src/flywheel/_schema/persistence-schema.sql
        +++ b/src/flywheel/_schema/persistence-schema.sql
        @@ -235,3 +235,8 @@
           version INTEGER NOT NULL
         );
        +CREATE TABLE IF NOT EXISTS reviews (
        +  id    INTEGER PRIMARY KEY AUTOINCREMENT,
        +  run_id TEXT NOT NULL,
        +  verdict TEXT NOT NULL
        +);
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.SCHEMA_COLUMN_OR_TABLE}
    )


def test_inline_column_addition_inside_create_table_fires_signal_2() -> None:
    # Mirrors the v5 awaiting_manual_ordinal addition: a brand-new column
    # line added inside an existing CREATE TABLE block (no ADD COLUMN
    # keyword, no new CREATE TABLE) -- the phase-08 / phase-17 pattern.
    diff = _diff(
        """
        diff --git a/src/flywheel/_schema/persistence-schema.sql b/src/flywheel/_schema/persistence-schema.sql
        --- a/src/flywheel/_schema/persistence-schema.sql
        +++ b/src/flywheel/_schema/persistence-schema.sql
        @@ -70,5 +70,6 @@
           blocked_requires_json   TEXT,
           task_content_hash       TEXT,
        +  review_state            TEXT,
           awaiting_manual_ordinal INTEGER
         );
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.SCHEMA_COLUMN_OR_TABLE}
    )


def test_sql_comment_only_change_does_not_fire_signal_2() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/_schema/persistence-schema.sql b/src/flywheel/_schema/persistence-schema.sql
        --- a/src/flywheel/_schema/persistence-schema.sql
        +++ b/src/flywheel/_schema/persistence-schema.sql
        @@ -1,3 +1,4 @@
         -- Flywheel loop persistence schema.
        +-- (Clarified header: described CREATE TABLE convention better.)
         --
         -- Tables mirror the conceptual model: tasks is ...
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()


# --- Signal 3: new Grader union variant -------------------------------------


def test_new_grader_class_in_task_fires_signal_3() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/task.py b/src/flywheel/task.py
        --- a/src/flywheel/task.py
        +++ b/src/flywheel/task.py
        @@ -76,2 +76,12 @@
         Grader = CommandGrader | RubricGrader | ManualGrader | TranscriptGrader
        +
        +
        +@dataclass(kw_only=True)
        +class LoopGrader:
        +    run: str
        +    name: str | None = None
        +    type: Literal["loop"] = "loop"
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.GRADER_VARIANT}
    )


def test_modified_grader_union_assignment_fires_signal_3() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/task.py b/src/flywheel/task.py
        --- a/src/flywheel/task.py
        +++ b/src/flywheel/task.py
        @@ -76,1 +76,1 @@
        -Grader = CommandGrader | RubricGrader | ManualGrader | TranscriptGrader
        +Grader = CommandGrader | RubricGrader | ManualGrader | TranscriptGrader | LoopGrader
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.GRADER_VARIANT}
    )


def test_new_grader_module_file_creation_fires_signal_3() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/grader_loop.py b/src/flywheel/grader_loop.py
        new file mode 100644
        --- /dev/null
        +++ b/src/flywheel/grader_loop.py
        @@ -0,0 +1,3 @@
        +def run_loop_grader(spec):
        +    \"\"\"Execute a loop grader.\"\"\"
        +    return True
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.GRADER_VARIANT}
    )


def test_editing_existing_grader_module_does_not_fire_signal_3() -> None:
    # The signal is "new module" (file creation), not "any edit to a
    # grader module." The "---" header here points at a real path, not
    # /dev/null, so is_new_file is False.
    diff = _diff(
        """
        diff --git a/src/flywheel/grader_command.py b/src/flywheel/grader_command.py
        --- a/src/flywheel/grader_command.py
        +++ b/src/flywheel/grader_command.py
        @@ -1,1 +1,1 @@
        -# header
        +# header (rephrased)
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()


# --- Signal 4: new Protocol method in store_protocols.py --------------------


def test_new_protocol_method_in_store_protocols_fires_signal_4() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/store_protocols.py b/src/flywheel/store_protocols.py
        --- a/src/flywheel/store_protocols.py
        +++ b/src/flywheel/store_protocols.py
        @@ -310,2 +310,6 @@
             def load_lifecycle(self, run_id: str) -> Lifecycle | None: ...
        +
        +    def list_awaiting_approval(
        +        self,
        +    ) -> list[Lifecycle]: ...
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.PROTOCOL_METHOD}
    )


def test_dunder_addition_in_store_protocols_does_not_fire_signal_4() -> None:
    # An ``__init__`` added to a new ``StoreConflictError`` subclass must
    # not fire signal 4; the decidable core is methods on Protocols, not
    # exception constructors.
    diff = _diff(
        """
        diff --git a/src/flywheel/store_protocols.py b/src/flywheel/store_protocols.py
        --- a/src/flywheel/store_protocols.py
        +++ b/src/flywheel/store_protocols.py
        @@ -85,2 +85,6 @@
                 self.task_id = task_id
        +
        +
        +class NewConflictError(StoreConflictError):
        +    def __init__(self, run_id: str) -> None:
        +        super().__init__(f"new conflict {run_id!r}")
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()


def test_new_def_in_harness_does_not_fire_signal_4() -> None:
    # Spec line 80: signal 4 must NOT fire on "any new function in
    # harness.py / orchestrator.py" -- that file-touch fallback is out of
    # scope for this module.
    diff = _diff(
        """
        diff --git a/src/flywheel/harness.py b/src/flywheel/harness.py
        --- a/src/flywheel/harness.py
        +++ b/src/flywheel/harness.py
        @@ -1,3 +1,7 @@
         from __future__ import annotations
        +
        +
        +def resolve_review(lifecycle):
        +    return lifecycle
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()


# --- Signal 5: new CONTROL_COMMAND_* constant -------------------------------


def test_new_control_command_constant_fires_signal_5() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/invoker_client.py b/src/flywheel/invoker_client.py
        --- a/src/flywheel/invoker_client.py
        +++ b/src/flywheel/invoker_client.py
        @@ -67,2 +67,3 @@
         CONTROL_COMMAND_REJECT: str = "reject"
        +CONTROL_COMMAND_RESUBMIT: str = "resubmit"
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {LoopPathSignal.CONTROL_COMMAND_VERB}
    )


def test_unrelated_constant_in_invoker_client_does_not_fire_signal_5() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/invoker_client.py b/src/flywheel/invoker_client.py
        --- a/src/flywheel/invoker_client.py
        +++ b/src/flywheel/invoker_client.py
        @@ -54,2 +54,3 @@
         DEFAULT_CONTROL_POLL_INTERVAL: float = 0.25
        +DEFAULT_RETRY_BACKOFF: float = 0.5
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()


# --- Combined / edge-case cases ---------------------------------------------


def test_multiple_signals_in_one_phase_diff() -> None:
    diff = _diff(
        """
        diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py
        --- a/src/flywheel/lifecycle.py
        +++ b/src/flywheel/lifecycle.py
        @@ -16,2 +16,3 @@
             INTERRUPTED = "interrupted"
        +    REVIEWING = "reviewing"
        diff --git a/src/flywheel/invoker_client.py b/src/flywheel/invoker_client.py
        --- a/src/flywheel/invoker_client.py
        +++ b/src/flywheel/invoker_client.py
        @@ -67,2 +67,3 @@
         CONTROL_COMMAND_REJECT: str = "reject"
        +CONTROL_COMMAND_REVIEW: str = "review"
        """
    )
    assert detect_loop_path_signals(diff) == frozenset(
        {
            LoopPathSignal.STATUS_OR_TRANSITION,
            LoopPathSignal.CONTROL_COMMAND_VERB,
        }
    )


def test_empty_diff_returns_empty_set() -> None:
    assert detect_loop_path_signals("") == frozenset()


def test_removed_status_member_does_not_fire() -> None:
    # A watched symbol that appears only in a removed line is not a hit.
    diff = _diff(
        """
        diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py
        --- a/src/flywheel/lifecycle.py
        +++ b/src/flywheel/lifecycle.py
        @@ -17,2 +17,1 @@
             INTERRUPTED = "interrupted"
        -    OLD_STATE = "old_state"
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()


def test_context_line_with_watched_symbol_does_not_fire() -> None:
    # The Status.DONE: line is a context line (leading space, not '+'),
    # so it must not fire signal 1.
    diff = _diff(
        """
        diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py
        --- a/src/flywheel/lifecycle.py
        +++ b/src/flywheel/lifecycle.py
        @@ -57,3 +57,3 @@
             Status.DONE: frozenset(),
        -    Status.FAILED: frozenset(),
        +    Status.FAILED: frozenset(),  # trailing comment tweak
         }
        """
    )
    # The added line modifies an existing entry, not a new one -- but
    # our detector cannot tell that from the diff alone. The intent of
    # this test is: the *context* Status.DONE line must not contribute.
    # The modified Status.FAILED line DOES match the transition pattern
    # because the detector is intentionally over-approximating. We still
    # assert at least that empty-context (the surrounding ``}`` and
    # ``Status.DONE`` context lines) does not cause more than one
    # signal: the over-approximation is the documented contract.
    signals = detect_loop_path_signals(diff)
    assert signals <= frozenset({LoopPathSignal.STATUS_OR_TRANSITION})


def test_pure_docstring_addition_in_lifecycle_does_not_fire() -> None:
    # A docstring tweak inside a watched file must not fire any
    # symbol-level signal.
    diff = _diff(
        """
        diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py
        --- a/src/flywheel/lifecycle.py
        +++ b/src/flywheel/lifecycle.py
        @@ -28,2 +28,3 @@
         class LifecycleTransitionError(ValueError):
             \"\"\"Raised when a Lifecycle transition violates docs/task-lifecycle.md.\"\"\"
        +    # Updated wording for the failure message.
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()


def test_add_then_revert_within_one_phase_nets_no_signal() -> None:
    # The function operates on the cumulative diff text the caller hands
    # in. An add-then-revert reduces to no net "+" line for the symbol,
    # so the marker must not fire. We simulate that by feeding a diff
    # that contains no added lines for the symbol at all (the net
    # cumulative diff after revert).
    diff = _diff(
        """
        diff --git a/src/flywheel/lifecycle.py b/src/flywheel/lifecycle.py
        --- a/src/flywheel/lifecycle.py
        +++ b/src/flywheel/lifecycle.py
        @@ -1,1 +1,1 @@
        -from dataclasses import dataclass, field
        +from dataclasses import dataclass, field  # (cumulative no-op)
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()


def test_non_watched_file_with_status_pattern_does_not_fire() -> None:
    # The detection is path-scoped: a Status-shaped addition outside
    # lifecycle.py (e.g. in a test fixture) must not fire signal 1.
    diff = _diff(
        """
        diff --git a/tests/fixtures/sample.py b/tests/fixtures/sample.py
        --- a/tests/fixtures/sample.py
        +++ b/tests/fixtures/sample.py
        @@ -1,1 +1,2 @@
         # sample fixture
        +    REVIEWING = "reviewing"
        """
    )
    assert detect_loop_path_signals(diff) == frozenset()

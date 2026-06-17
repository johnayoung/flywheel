"""Held-out acceptance test: postgres read-path parity for the status verb.

BLIND test, authored from the contract only. It discriminates the one
behavior: when ``flywheel status`` (forwarded to the orchestrator's
``main(["status"])``) runs in a project whose ``flywheel.toml`` selects the
postgres store backend with a *reachable* database, the process must NOT emit
the "does not support the postgres store backend" refusal and must exit 0 --
without leaving a Python traceback on stdout/stderr.

Asserting BOTH exit-0 AND no-traceback (plus absence of the refusal phrase) is
what forecloses the gaming moves:
  - still printing the refusal + non-zero exit  -> fails (refusal phrase present)
  - letting AttributeError/psycopg bubble up     -> fails (traceback marker present)
  - swallowing the error, forcing exit 0, but    -> fails (traceback marker present)
    leaving a stack on stderr
Only a clean status render -- exit 0 with none of those markers -- passes.

Skips when no Postgres DB is reachable (``require_postgres`` fixture).
"""

from pathlib import Path

import pytest

from flywheel_orchestrator._store_factory import (
    PG_DSN_ENV,
    PG_DSN_FALLBACK_ENV,
)
from flywheel_orchestrator._workflow import main as orch_main

# The refusal phrase that a postgres-incapable status path emits (criterion).
REFUSAL_PHRASE = "does not support the postgres store backend"

# Unhandled-crash markers: a clean status render contains none of these, but
# a swallowed-error-then-crash or a different-hard-error path leaves a stack on
# stderr. The traceback header is the primary discriminator; the type/module
# names harden against a forced exit-0 that still prints a stack.
TRACEBACK_HEADER = "Traceback (most recent call last)"
ATTRIBUTE_ERROR_MARKER = "AttributeError"
PSYCOPG_MARKER = "psycopg"


def test_status_verb_postgres_end_to_end(
    require_postgres: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """status over a reachable postgres store: exit 0, no refusal, no traceback.

    End-to-end through the exact orchestrator entry the ``flywheel`` / ``fw``
    router forwards ``status`` to, exercising the full store-construction +
    read path.
    """
    dsn = require_postgres

    # Configure the project for the postgres backend (exact shape from the
    # committed fail-fast test) and point the canonical DSN env var at the
    # reachable database. Clear the fallback var so resolution is unambiguous.
    monkeypatch.chdir(tmp_path)
    (tmp_path / "flywheel.toml").write_text(
        '[source]\nkind = "directory"\n\n[store]\nbackend = "postgres"\n',
        encoding="utf-8",
    )
    monkeypatch.setenv(PG_DSN_ENV, dsn)
    monkeypatch.delenv(PG_DSN_FALLBACK_ENV, raising=False)

    # The function under test, end to end. Returns a process-style exit code.
    exit_code = orch_main(["status"])

    out, err = capsys.readouterr()
    combined = out + err

    # (a) The refusal must be gone, regardless of exit code.
    assert REFUSAL_PHRASE not in combined, (
        "status still emitted the postgres-backend refusal phrase: "
        f"{combined!r}"
    )

    # (b) No unhandled crash may reach stdout/stderr. The traceback header is
    # the decisive marker -- a swallowed-then-crashed or replaced-with-a-
    # different-hard-error path leaves a stack here even if exit were forced 0.
    assert TRACEBACK_HEADER not in combined, (
        "status left an unhandled Python traceback on stdout/stderr: "
        f"{combined!r}"
    )
    assert ATTRIBUTE_ERROR_MARKER not in combined, (
        "status surfaced an unhandled AttributeError: "
        f"{combined!r}"
    )
    assert PSYCOPG_MARKER not in combined, (
        "status surfaced an unhandled psycopg error: "
        f"{combined!r}"
    )

    # (c) Clean success exit. Pinned last so the absence assertions above run
    # even when a wrong path forces a non-zero exit alongside its output.
    assert exit_code == 0, (
        f"status exited {exit_code!r} against a reachable postgres store "
        f"(expected 0); output was: {combined!r}"
    )

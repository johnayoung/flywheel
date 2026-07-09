"""``flywheel`` / ``fw`` command router.

Bare ``flywheel`` (or ``fw``) opens the Textual console; ``--json`` or a
non-TTY stdout prints one JSON snapshot and exits (delegated to
:func:`flywheel._tui.main`, which already implements that fork).
Every other verb forwards its remaining argv to the pre-existing
subcommand implementation so output and exit codes match the
pre-router originals byte-for-byte. ``flywheel`` and ``fw`` are byte-
identical entries on this one implementation.

The verb map is fixed by spec 00021's FR-4:

* orchestrator: ``init``, ``status``, ``live``, ``archive``,
  ``recover``, ``resolve``, ``recheck-blocked``
* core (single-task producer verbs): ``interrupt``, ``approve``,
  ``reject``, ``say`` (surface name) -> ``steer`` (core verb)
* worker: ``worker [--once]`` -> the git-worktree daemon loop in
  :mod:`flywheel_worktree.worker`
* audit: ``audit RUN_ID`` -> ``python -m flywheel_core.audit``

``run``, ``next``, and bare ``orchestrate`` are intentionally NOT
routed -- the spec keeps them as ``python -m`` plumbing and
``worker --once`` as the only blessed headless drain.
"""

from __future__ import annotations

import sys
from collections.abc import Sequence

from flywheel_core.audit._cli import main as _audit_main
from flywheel_core.workflow import main as _core_main
from flywheel_orchestrator import (
    PolicyError,
    load_effective_policy,
    open_sqlite_bound_store,
    resolve_db_path,
)
from flywheel_orchestrator._autopilot_run import main as _autopilot_main
from flywheel_orchestrator._workflow import main as _orchestrator_main
from flywheel_worktree.worker import main as _worker_main

from flywheel._docs import main as _docs_main
from flywheel._tui import main as _tui_main


_PROG = "fw"

# Verbs delegated verbatim to the orchestrator CLI (the multi-task
# layer): the orchestrator's parser already owns their flags
# (``--policy`` / ``--tasks-dir`` / ``--db`` / ``--json`` / ``--watch``)
# and exit codes, so the router just hands the argv through.
_ORCHESTRATOR_VERBS: frozenset[str] = frozenset(
    {
        "init",
        "status",
        "live",
        "history",
        "show",
        "archive",
        "recover",
        "resolve",
        "recheck-blocked",
        "validate",
    }
)

# Producer verbs that enqueue control commands against an existing
# lifecycle. ``say`` is the surface alias for core's ``steer`` verb
# (per spec FR-4); the rename happens here so the underlying core CLI
# is unchanged.
_CORE_VERBS: frozenset[str] = frozenset({"interrupt", "approve", "reject"})
_SAY_VERB = "say"
_CORE_STEER_VERB = "steer"

_WORKER_VERB = "worker"
_AUTOPILOT_VERB = "autopilot"
_AUDIT_VERB = "audit"
_DOCS_VERB = "docs"

_USAGE = """\
usage: flywheel [--json] [--help] <verb> [args...]
       fw [--json] [--help] <verb> [args...]

'flywheel' and 'fw' are byte-identical entries on one implementation.
Bare invocation opens the operator console in a TTY; '--json' (or any
invocation whose stdout is not a TTY) prints one JSON snapshot and
exits 0.

Verbs:
  init             scaffold .flywheel/ and a flywheel.toml work policy
  worker [--once]  run the git-worktree daemon loop (or a single drain)
  autopilot        keep the queue full (neverending; --once for one pass)
  status [--json]  print every active task's state
  live [--watch N] print one line per in-flight run (optionally tailing)
  history [--json] list finished runs, one line per task, newest first
  show ID [--json] show one run in full (accepts a run_id or task id)
  say RUN_ID MSG   inject an operator message into a live run
  interrupt RUN_ID interrupt a live run
  approve RUN_ID   approve a parked manual-approval gate
  reject RUN_ID    reject a parked manual-approval gate (--feedback TEXT)
  archive          move done phases out of active/
  recover          finalize stranded lifecycles
  resolve ID        abandon a strand (--reason TEXT) so its phase archives
  recheck-blocked  re-evaluate blocked lifecycles' requires
  validate         statically validate active tasks' graders (lint)
  audit RUN_ID     stream the totally-ordered audit records for a run
  docs [TOPIC]     print a curated operator doc (bare: list the topics)

Each verb forwards its own --help to the underlying implementation,
so 'flywheel <verb> --help' (equivalently 'fw <verb> --help') lists
that verb's specific flags.
"""


def _delegate_audit(rest: list[str]) -> int:
    """Forward ``audit`` to core's CLI with the product's db resolution.

    Core's audit plumbing defaults ``--db`` to ``$FLYWHEEL_DB`` or
    ``./flywheel.db``; every sibling verb resolves the store as ``--db``
    > policy ``[paths] db`` > ``.flywheel/flywheel.sqlite``. Inject the
    resolved path when the operator did not pass ``--db`` so ``fw audit``
    reads the same store ``fw history`` / ``fw show`` do. An explicit
    ``--db`` (or a bare ``--help``) forwards untouched.
    """
    if any(a == "--db" or a.startswith("--db=") for a in rest) or any(
        a in ("-h", "--help") for a in rest
    ):
        return _audit_main(rest)
    try:
        policy = load_effective_policy(None)
    except PolicyError as exc:
        print(f"{_PROG}: policy error: {exc}", file=sys.stderr)
        return 2
    db_path = resolve_db_path(None, policy=policy)
    return _audit_main(["--db", str(db_path), *rest])


def _delegate_producer(core_argv: list[str]) -> int:
    """Forward a producer verb to core, injecting the policy-selected store.

    The core producer verbs (interrupt/steer/set-model/approve/reject)
    enqueue a control command. Core opens a :class:`SqliteStore` at the
    resolved ``--db`` by default; under a postgres ``[store]`` policy that
    would persist the command to the wrong backend and dual-write a stray
    sqlite file. So when the effective policy selects a non-sqlite backend,
    build that store here -- the layer that owns policy and the DSN
    environment -- and inject it through core's ``store`` seam (spec
    00075), so the command lands in postgres.

    A sqlite policy (or no policy) forwards untouched: the default path
    stays byte-identical, ``--db`` semantics and all. A bare ``--help``
    forwards untouched too so help never requires a store (or a database
    connection). Postgres fail-fast configuration errors -- no DSN, a
    missing extra -- surface as the factory's :class:`StoreConfigError`
    (a :class:`PolicyError` subclass): printed to stderr, exit 2, never a
    silent sqlite fallback.
    """
    if any(a in ("-h", "--help") for a in core_argv[1:]):
        return _core_main(core_argv)
    try:
        policy = load_effective_policy(None)
    except PolicyError as exc:
        print(f"{_PROG}: policy error: {exc}", file=sys.stderr)
        return 2
    backend = policy.store_backend if policy is not None else "sqlite"
    if backend == "sqlite":
        # Byte-identical default: core resolves --db and opens its own
        # SqliteStore exactly as before.
        return _core_main(core_argv)
    db_path = resolve_db_path(None, policy=policy)
    try:
        store = open_sqlite_bound_store(policy, db_path=db_path)
    except PolicyError as exc:
        # StoreConfigError (no DSN / missing extra) subclasses PolicyError.
        print(f"{_PROG}: {exc}", file=sys.stderr)
        return 2
    try:
        return _core_main(core_argv, store=store)
    finally:
        store.close()


def main(argv: Sequence[str] | None = None) -> int:
    """Route ``flywheel <verb> ...`` (or ``fw <verb> ...``) to the
    pre-existing implementation.

    Returns the delegated command's exit code unchanged. Unknown verbs
    exit 2 (argparse convention) with a stderr line pointing at
    ``fw --help``.
    """
    args = list(sys.argv[1:] if argv is None else argv)

    # Bare ``fw`` -> the TUI's TTY-vs--json fork already owns that
    # decision; hand it the empty argv so its parser picks defaults.
    if not args:
        return _tui_main([])

    first = args[0]

    if first in ("-h", "--help"):
        print(_USAGE, end="")
        return 0

    # Top-level flags without a verb (``fw --json``, ``fw --db ...``)
    # remain TUI flags -- the TUI's parser already accepts them.
    if first.startswith("-"):
        return _tui_main(args)

    verb = first
    rest = args[1:]

    if verb in _ORCHESTRATOR_VERBS:
        return _orchestrator_main([verb, *rest])
    if verb in _CORE_VERBS:
        # Producer verbs enqueue a control command; route through the
        # store-injecting delegate so a postgres [store] policy lands the
        # command in postgres instead of a stray sqlite file.
        return _delegate_producer([verb, *rest])
    if verb == _SAY_VERB:
        # Surface name rename: the shell's 'say' is core's 'steer' verb.
        return _delegate_producer([_CORE_STEER_VERB, *rest])
    if verb == _WORKER_VERB:
        # In-process delegation to the git-worktree daemon loop in
        # :mod:`flywheel_worktree.worker` (spec constraint: no shell-out).
        return _worker_main(rest)
    if verb == _AUTOPILOT_VERB:
        # The autopilot intake daemon: keeps the work queue full with
        # verifiable, tier-prioritized tasks. Neverending by default;
        # --once runs a single refill pass.
        return _autopilot_main(rest)
    if verb == _AUDIT_VERB:
        return _delegate_audit(rest)
    if verb == _DOCS_VERB:
        # Curated operator docs served from package data -- knowledge, not
        # policy: no flywheel.toml load, no store, works from any directory.
        return _docs_main(rest)

    print(f"{_PROG}: unknown command: {verb}", file=sys.stderr)
    print(f"  see '{_PROG} --help' for the verb list", file=sys.stderr)
    return 2


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())

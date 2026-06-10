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
  ``recover``, ``recheck-blocked``
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
from flywheel_orchestrator._workflow import main as _orchestrator_main
from flywheel_worktree.worker import main as _worker_main

from flywheel._tui import main as _tui_main


_PROG = "fw"

# Verbs delegated verbatim to the orchestrator CLI (the multi-task
# layer): the orchestrator's parser already owns their flags
# (``--policy`` / ``--tasks-dir`` / ``--db`` / ``--json`` / ``--watch``)
# and exit codes, so the router just hands the argv through.
_ORCHESTRATOR_VERBS: frozenset[str] = frozenset(
    {"init", "status", "live", "archive", "recover", "recheck-blocked"}
)

# Producer verbs that enqueue control commands against an existing
# lifecycle. ``say`` is the surface alias for core's ``steer`` verb
# (per spec FR-4); the rename happens here so the underlying core CLI
# is unchanged.
_CORE_VERBS: frozenset[str] = frozenset({"interrupt", "approve", "reject"})
_SAY_VERB = "say"
_CORE_STEER_VERB = "steer"

_WORKER_VERB = "worker"
_AUDIT_VERB = "audit"

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
  status [--json]  print every active task's state
  live [--watch N] print one line per in-flight run (optionally tailing)
  say RUN_ID MSG   inject an operator message into a live run
  interrupt RUN_ID interrupt a live run
  approve RUN_ID   approve a parked manual-approval gate
  reject RUN_ID    reject a parked manual-approval gate (--feedback TEXT)
  archive          move done phases out of active/
  recover          finalize stranded lifecycles
  recheck-blocked  re-evaluate blocked lifecycles' requires
  audit RUN_ID     stream the totally-ordered audit records for a run

Each verb forwards its own --help to the underlying implementation,
so 'flywheel <verb> --help' (equivalently 'fw <verb> --help') lists
that verb's specific flags.
"""


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
        return _core_main([verb, *rest])
    if verb == _SAY_VERB:
        # Surface name rename: the shell's 'say' is core's 'steer' verb.
        return _core_main([_CORE_STEER_VERB, *rest])
    if verb == _WORKER_VERB:
        # In-process delegation to the git-worktree daemon loop in
        # :mod:`flywheel_worktree.worker` (spec constraint: no shell-out).
        return _worker_main(rest)
    if verb == _AUDIT_VERB:
        return _audit_main(rest)

    print(f"{_PROG}: unknown command: {verb}", file=sys.stderr)
    print(f"  see '{_PROG} --help' for the verb list", file=sys.stderr)
    return 2


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())

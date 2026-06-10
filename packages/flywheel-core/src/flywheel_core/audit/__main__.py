"""``python -m flywheel_core.audit`` entry point.

Delegates to :func:`flywheel_core.audit._cli.main` and propagates its exit
code. Kept intentionally minimal so the CLI surface is testable without
``-m`` -- importers can call :func:`flywheel_core.audit._cli.main` directly.
"""

from __future__ import annotations

from flywheel_core.audit._cli import main


if __name__ == "__main__":
    raise SystemExit(main())

"""Claude-specific fault recognition.

Ports the vendor-string knowledge from ``flywheel_core.faults`` to its proper
owner, the adapter. Two recognized forms of the session-limit refusal:

- ``"Claude AI usage limit reached|<epoch>"`` — carries the absolute reset
  instant (epoch seconds, UTC) on ``resets_at_epoch``.
- Any other ``"usage limit reached"`` text (e.g. the human ``"resets 6pm"``
  variant) — reported without ``resets_at_epoch``; the wall-clock form is
  relative to the observer's timezone, so the caller derives the absolute
  reset with its own ``now`` (``flywheel_core.faults`` keeps that logic).
"""

from __future__ import annotations

import re

from flywheel_agents.models import AgentFault, FaultEvidence

# ``Claude AI usage limit reached|1751990400`` (epoch seconds, UTC).
_PIPE_EPOCH_RE = re.compile(r"usage limit reached\s*\|\s*(\d+)", re.IGNORECASE)

_LIMIT_MARKER_RE = re.compile(r"usage limit reached", re.IGNORECASE)


def classify_claude_fault(evidence: FaultEvidence) -> AgentFault | None:
    for source in (evidence.final_text or "", evidence.stderr or ""):
        if not source:
            continue
        pipe = _PIPE_EPOCH_RE.search(source)
        if pipe is not None:
            return AgentFault(
                kind="session_limit",
                message="Claude usage limit reached (pipe-epoch refusal)",
                resets_at_epoch=float(pipe.group(1)),
            )
        if _LIMIT_MARKER_RE.search(source):
            return AgentFault(
                kind="session_limit",
                message="Claude usage limit reached",
            )
    return None

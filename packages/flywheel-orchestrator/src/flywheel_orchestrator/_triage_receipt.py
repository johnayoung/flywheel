"""Triage receipt: the schema + content-hash the github drain trusts.

A *triage receipt* attests that an issue's human-authored content was seen by a
triage run which produced a fail-first grader — a command that exits non-zero
against a named base. The receipt is embedded in the issue's ```` ```flywheel ````
spec block under the :data:`RECEIPT_KEY` key. The drain-side trust rule
(:class:`~flywheel_orchestrator._github.GithubWorkSource` built with
``require_triage_receipt``) schedules an issue only when its receipt's
``content_hash`` matches the issue's *current* human content, so a human edit
to the title or prose after triage invalidates a stale receipt.

The content hash covers the issue title plus the body **with the ``flywheel``
fenced block stripped**, so the machine-managed spec block (which carries the
receipt itself) never feeds its own hash. Stripping is normalized with
:meth:`str.strip` at the boundaries so a stray trailing separator added when
the block was embedded does not perturb the hash.

This module is the single home of both the receipt schema and the content-hash
definition. The triage *engine* (authored later, on the write side) imports the
same definitions, so the side that mints a receipt and the side that trusts it
can never drift. It imports nothing from the drain
(:mod:`flywheel_orchestrator._github`); the trust *policy* — what a stale or
missing receipt means for scheduling — lives there, not here.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass

#: The key under which a receipt lives inside the ``flywheel`` spec block.
RECEIPT_KEY = "triage_receipt"

# The fence markers of the ``flywheel`` spec block. Duplicated as trivial
# literals rather than imported from the drain so this module stays free of any
# dependency on _github (the triage engine imports this module, not the drain).
_SPEC_FENCE_OPEN = "```flywheel"
_SPEC_FENCE_CLOSE = "```"


@dataclass(frozen=True)
class TriageReceipt:
    """A parsed, well-typed triage receipt.

    ``command`` is the fail-first grader command triage ran; ``exit_code`` its
    exit status (non-zero is the fail-first invariant — see
    :func:`is_fail_first`); ``base_sha`` the commit the command was run
    against; ``content_hash`` the :func:`content_hash` of the issue's human
    content at triage time.
    """

    command: str
    exit_code: int
    base_sha: str
    content_hash: str


def strip_spec_block(body: str) -> str:
    """Return ``body`` with the first ``flywheel`` fenced block removed.

    Removes the region from the opening ```` ```flywheel ```` fence line through
    its closing ```` ``` ```` fence line, inclusive. An unterminated block (no
    closing fence) drops everything from the open fence to the end. A body with
    no block is returned with only its boundary whitespace normalized. The
    result is :meth:`str.strip`-normalized so the blank separator that typically
    precedes an embedded block does not leak into the hashed material.
    """
    lines = body.splitlines()
    open_idx: int | None = None
    for idx, line in enumerate(lines):
        if line.strip() == _SPEC_FENCE_OPEN:
            open_idx = idx
            break
    if open_idx is None:
        return body.strip()
    close_idx: int | None = None
    for idx in range(open_idx + 1, len(lines)):
        if lines[idx].strip() == _SPEC_FENCE_CLOSE:
            close_idx = idx
            break
    if close_idx is None:
        remaining = lines[:open_idx]
    else:
        remaining = lines[:open_idx] + lines[close_idx + 1 :]
    return "\n".join(remaining).strip()


def content_hash(title: str, body: str) -> str:
    """The canonical content hash of an issue's human-authored content.

    Defined as the SHA-256 of the issue ``title`` joined with the ``body``
    **after the ``flywheel`` spec block is stripped** (:func:`strip_spec_block`).
    Both the write side (the triage engine, which mints the receipt) and the
    drain side (:class:`~flywheel_orchestrator._github.GithubWorkSource`) call
    this one function, so a receipt minted before the block was embedded hashes
    identically to the same issue re-read with the block present.
    """
    material = f"{title.strip()}\n{strip_spec_block(body)}"
    return hashlib.sha256(material.encode("utf-8")).hexdigest()


def parse_receipt(value: object) -> TriageReceipt | None:
    """Parse a spec block's :data:`RECEIPT_KEY` value into a :class:`TriageReceipt`.

    Returns ``None`` when ``value`` is not a well-formed receipt object: not a
    mapping, or any of the four fields missing or wrong-typed (``command`` and
    ``base_sha``/``content_hash`` must be non-empty strings; ``exit_code`` must
    be an ``int`` — a JSON ``bool`` is rejected, since ``bool`` is an ``int``
    subclass). Validity of the *contents* (fail-first, hash currency) is the
    caller's decision, not a parse concern.
    """
    if not isinstance(value, dict):
        return None
    command = value.get("command")
    exit_code = value.get("exit_code")
    base_sha = value.get("base_sha")
    hashed = value.get("content_hash")
    if not isinstance(command, str) or not command.strip():
        return None
    if isinstance(exit_code, bool) or not isinstance(exit_code, int):
        return None
    if not isinstance(base_sha, str) or not base_sha.strip():
        return None
    if not isinstance(hashed, str) or not hashed.strip():
        return None
    return TriageReceipt(
        command=command,
        exit_code=exit_code,
        base_sha=base_sha,
        content_hash=hashed,
    )


def is_fail_first(receipt: TriageReceipt) -> bool:
    """Whether ``receipt`` records a fail-first grader (non-zero exit).

    A receipt whose ``exit_code`` is ``0`` records a command that *passed* at
    triage time — there is nothing to fix, so it is not a valid basis for
    scheduling work.
    """
    return receipt.exit_code != 0


__all__ = [
    "RECEIPT_KEY",
    "TriageReceipt",
    "content_hash",
    "is_fail_first",
    "parse_receipt",
    "strip_spec_block",
]

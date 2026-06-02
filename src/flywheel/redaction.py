"""Pure redaction layer over the audit stream.

Wiring-agnostic transforms over a single :class:`AuditRecord`. The same
:class:`Redactor` object is reusable at the read seam
(``flywheel.audit.stream``) and at the persistence seam (write-time, a
follow-up feature) without modification: the protocol takes one record in
and returns one record out, with no reference to streams, cursors, or the
store.

Redaction is best-effort secret suppression, not a guarantee. Unmatched
secrets can still pass through; the store remains the unredacted source of
truth for forensics. Callers wanting verbatim data opt out.

Purity discipline (mirrors :mod:`flywheel.task` / :mod:`flywheel.lifecycle`):
this module imports no ``json``, ``pathlib``, or ``io``, never calls
``open()``, and the core transform performs no I/O. Built-in redactors that
need ambient state (e.g. an env-value redactor) read it at construction time
in their dedicated module, not here.
"""

from __future__ import annotations

import dataclasses
import re
from collections.abc import Callable, Mapping, Sequence
from typing import Any, Protocol, runtime_checkable

from flywheel.store_protocols import AuditRecord


@runtime_checkable
class Redactor(Protocol):
    """A pure transform over a single :class:`AuditRecord`.

    Implementations MUST:

    * Return a record of the same concrete type as the input
      (``EventRecord`` stays ``EventRecord``; ``SdkMessageRecord`` stays
      ``SdkMessageRecord``).
    * Leave ``sequence``, ``id``, ``ts``, ``run_id``, and every non-payload
      field unchanged.
    * Never mutate the input record or any nested container inside its
      payload; produce fresh containers instead.
    * Perform no I/O.

    The protocol intentionally references only :class:`AuditRecord` — not
    the store, not the stream, not any cursor — so the same object can be
    applied at read time (the MVP wiring) or at the persistence seam (a
    follow-up) without change.
    """

    def redact(self, record: AuditRecord) -> AuditRecord: ...


# --- Deep traversal helper --------------------------------------------------


def _walk(value: Any, replace: Callable[[str], str]) -> Any:
    """Return a deep copy of ``value`` with every string leaf passed
    through ``replace``.

    Mappings become fresh ``dict``s; ``list``s become fresh ``list``s;
    ``tuple``s become fresh ``tuple``s. Every non-string leaf (``int``,
    ``bool``, ``None``, ``float``, ``bytes``, etc.) passes through
    untouched. ``str`` is intentionally checked *before* the
    ``Sequence``-friendly branches because a ``str`` is itself a
    ``Sequence`` and must not be split into characters.
    """

    if isinstance(value, str):
        return replace(value)
    if isinstance(value, Mapping):
        return {key: _walk(item, replace) for key, item in value.items()}
    if isinstance(value, list):
        return [_walk(item, replace) for item in value]
    if isinstance(value, tuple):
        return tuple(_walk(item, replace) for item in value)
    return value


def _rebuild(record: AuditRecord, replace: Callable[[str], str]) -> AuditRecord:
    """Return a new record of ``record``'s concrete type with the payload
    rewritten by ``replace`` and every other field copied verbatim."""

    new_payload = _walk(record.payload, replace)
    return dataclasses.replace(record, payload=new_payload)


# --- PatternRedactor --------------------------------------------------------


class PatternRedactor:
    """Replaces substrings matching configured regexes inside any string
    leaf of a record's payload.

    Construction takes an iterable of ``(label, pattern)`` pairs; each
    pattern is compiled once (NFR-2) and reused across every record.
    Matched substrings are replaced by ``[REDACTED:<label>]`` in-place
    inside the surrounding string, so a secret embedded in a larger log
    line leaves the rest of the line intact.

    The order of patterns is preserved: earlier rules apply first, so a
    later pattern sees the output of the earlier one. This is the same
    contract :func:`compose` provides between redactors and lets callers
    layer specific labels on top of generic catch-alls.
    """

    __slots__ = ("_patterns",)

    def __init__(self, patterns: Sequence[tuple[str, str]]) -> None:
        compiled: list[tuple[str, re.Pattern[str]]] = []
        for label, pattern in patterns:
            if not isinstance(label, str) or not label:
                raise ValueError(
                    "PatternRedactor pattern label must be a non-empty string"
                )
            try:
                compiled.append((label, re.compile(pattern)))
            except re.error as exc:
                raise ValueError(
                    f"invalid regex for label {label!r}: {exc}"
                ) from exc
        self._patterns: tuple[tuple[str, re.Pattern[str]], ...] = tuple(compiled)

    def redact(self, record: AuditRecord) -> AuditRecord:
        return _rebuild(record, self._replace)

    def _replace(self, text: str) -> str:
        out = text
        for label, pattern in self._patterns:
            out = pattern.sub(f"[REDACTED:{label}]", out)
        return out


# --- Composition ------------------------------------------------------------


class _ComposedRedactor:
    """Applies a fixed tuple of redactors in order. Implementation detail of
    :func:`compose`; exposed only via the :class:`Redactor` protocol."""

    __slots__ = ("_redactors",)

    def __init__(self, redactors: tuple[Redactor, ...]) -> None:
        self._redactors = redactors

    def redact(self, record: AuditRecord) -> AuditRecord:
        current = record
        for redactor in self._redactors:
            current = redactor.redact(current)
        return current


def compose(*redactors: Redactor) -> Redactor:
    """Return a :class:`Redactor` that applies each of ``redactors`` in
    order; the output of one feeds the next.

    Order is part of the contract: ``compose(a, b)`` always runs ``a``
    first, then ``b`` on ``a``'s output, deterministically. ``compose()``
    with no arguments is the identity redactor.
    """

    return _ComposedRedactor(tuple(redactors))


__all__ = [
    "PatternRedactor",
    "Redactor",
    "compose",
]

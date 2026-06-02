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
``open()``, and the core transform performs no I/O. The one accepted impurity
is :class:`EnvValueRedactor`'s constructor, which reads ``os.environ`` at
construction time and captures the resulting values in memory; once
constructed the redactor is a pure transform like every other built-in.
"""

from __future__ import annotations

import dataclasses
import hashlib
import hmac
import os
import re
from collections.abc import Callable, Mapping, Sequence
from typing import Any, Protocol, runtime_checkable

from flywheel.store_protocols import AuditRecord, SdkMessageRecord


# --- Stable-token helper ----------------------------------------------------


_DIGEST_HEX_LEN = 8


def _coerce_salt(salt: bytes | str | None) -> bytes | None:
    """Normalize a caller-supplied salt to ``bytes`` (or ``None``).

    ``str`` salts are encoded as UTF-8 for ergonomic call sites that want to
    write ``salt="my-salt"`` instead of ``salt=b"my-salt"``. Any other type
    is a programmer error and raises :class:`TypeError` at construction.
    """

    if salt is None:
        return None
    if isinstance(salt, bytes):
        return salt
    if isinstance(salt, str):
        return salt.encode("utf-8")
    raise TypeError("salt must be bytes, str, or None")


def _format_token(label: str, cleartext: str | None, salt: bytes | None) -> str:
    """Render the placeholder token for one redacted occurrence.

    Without a salt (or when the redactor has no string cleartext to hash)
    the token is the bare ``[REDACTED:<label>]``. With a salt the token is
    ``[REDACTED:<label>:<digest>]`` where ``<digest>`` is the first
    :data:`_DIGEST_HEX_LEN` hex chars of HMAC-SHA256 over ``cleartext``
    keyed by ``salt`` (FR-8). The digest is one-way; the cleartext is never
    recoverable from a token.

    Same ``(label, cleartext, salt)`` always yields the same token across
    records and processes (NFR-3) so consumers can correlate occurrences
    of the same secret without ever seeing it.
    """

    if salt is None or cleartext is None:
        return f"[REDACTED:{label}]"
    digest = hmac.new(salt, cleartext.encode("utf-8"), hashlib.sha256).hexdigest()
    return f"[REDACTED:{label}:{digest[:_DIGEST_HEX_LEN]}]"


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


# --- Deep traversal helpers -------------------------------------------------


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


def _deep_copy(value: Any) -> Any:
    """Return a deep copy of ``value`` with every leaf preserved exactly.

    Same container rules as :func:`_walk` but the identity transform on
    leaves. Used by redactors that mutate the copy in place
    (:class:`FieldPathRedactor`, :class:`ToolDenylistRedactor`).
    """

    if isinstance(value, Mapping):
        return {key: _deep_copy(item) for key, item in value.items()}
    if isinstance(value, list):
        return [_deep_copy(item) for item in value]
    if isinstance(value, tuple):
        return tuple(_deep_copy(item) for item in value)
    return value


# --- PatternRedactor --------------------------------------------------------


class PatternRedactor:
    """Replaces substrings matching configured regexes inside any string
    leaf of a record's payload.

    Construction takes an iterable of ``(label, pattern)`` pairs; each
    pattern is compiled once (NFR-2) and reused across every record.
    Matched substrings are replaced by ``[REDACTED:<label>]`` (or
    ``[REDACTED:<label>:<digest>]`` when ``salt`` is configured; FR-8)
    in-place inside the surrounding string, so a secret embedded in a
    larger log line leaves the rest of the line intact.

    The order of patterns is preserved: earlier rules apply first, so a
    later pattern sees the output of the earlier one. This is the same
    contract :func:`compose` provides between redactors and lets callers
    layer specific labels on top of generic catch-alls.
    """

    __slots__ = ("_patterns", "_salt")

    def __init__(
        self,
        patterns: Sequence[tuple[str, str]],
        *,
        salt: bytes | str | None = None,
    ) -> None:
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
        self._salt: bytes | None = _coerce_salt(salt)

    def redact(self, record: AuditRecord) -> AuditRecord:
        return _rebuild(record, self._replace)

    def _replace(self, text: str) -> str:
        out = text
        for label, pattern in self._patterns:
            if self._salt is None:
                out = pattern.sub(f"[REDACTED:{label}]", out)
            else:
                out = pattern.sub(self._make_sub(label), out)
        return out

    def _make_sub(self, label: str) -> Callable[[re.Match[str]], str]:
        salt = self._salt

        def _sub(match: re.Match[str]) -> str:
            return _format_token(label, match.group(0), salt)

        return _sub


# --- EnvValueRedactor -------------------------------------------------------


class EnvValueRedactor:
    """Redacts the literal values of named environment variables wherever
    they appear in a payload string.

    The redactor reads ``os.environ`` exactly once, at construction time
    (FR-4), and captures each named variable's value in memory. From that
    point on it is a pure transform: changing the process environment
    later does not change what it redacts.

    Guards against degenerate cases that would otherwise blank everything
    out:

    * An env var that is absent contributes no pattern.
    * An env var whose value is the empty string contributes no pattern
      (otherwise ``"".replace`` would inject the token between every
      character).
    * An env var whose value is shorter than ``minimum_length`` is
      excluded; this stops common short substrings (e.g. a one-character
      flag) from being treated as a secret.

    Matching is literal substring matching, not regex, so values that
    look like regex metacharacters are handled correctly. With a salt
    configured the placeholder includes a stable HMAC digest of the
    captured value so identical secrets correlate across records
    (FR-8 / NFR-3).
    """

    DEFAULT_MINIMUM_LENGTH: int = 8

    __slots__ = ("_label_values", "_salt")

    def __init__(
        self,
        var_names: Sequence[str],
        *,
        salt: bytes | str | None = None,
        minimum_length: int = DEFAULT_MINIMUM_LENGTH,
        labels: Mapping[str, str] | None = None,
    ) -> None:
        if minimum_length < 1:
            raise ValueError("EnvValueRedactor minimum_length must be >= 1")
        captured: list[tuple[str, str]] = []
        seen: set[str] = set()
        for name in var_names:
            if not isinstance(name, str) or not name:
                raise ValueError(
                    "EnvValueRedactor var_names must be non-empty strings"
                )
            if name in seen:
                continue
            seen.add(name)
            value = os.environ.get(name)
            if not value:  # absent or empty string
                continue
            if len(value) < minimum_length:
                continue
            label = (labels or {}).get(name, name)
            if not isinstance(label, str) or not label:
                raise ValueError(
                    "EnvValueRedactor labels values must be non-empty strings"
                )
            captured.append((label, value))
        # Sort by value length descending so a long captured value is
        # redacted before any shorter value it might contain. This avoids
        # leaking the long value's prefix when two captured values overlap.
        captured.sort(key=lambda lv: -len(lv[1]))
        self._label_values: tuple[tuple[str, str], ...] = tuple(captured)
        self._salt: bytes | None = _coerce_salt(salt)

    def redact(self, record: AuditRecord) -> AuditRecord:
        return _rebuild(record, self._replace)

    def _replace(self, text: str) -> str:
        if not self._label_values:
            return text
        out = text
        for label, value in self._label_values:
            if value and value in out:
                token = _format_token(label, value, self._salt)
                out = out.replace(value, token)
        return out


# --- FieldPathRedactor ------------------------------------------------------


class FieldPathRedactor:
    """Redacts the entire value at operator-named payload key paths.

    Paths use ``.`` as a segment separator and ``*`` as a wildcard for
    every element of a list. Examples:

    * ``"env"`` — replace the value at ``payload["env"]``.
    * ``"tool_input.command"`` — replace
      ``payload["tool_input"]["command"]``.
    * ``"items.*.secret"`` — for each element of ``payload["items"]``,
      replace its ``"secret"`` field.

    A path that does not exist in a particular payload is a silent no-op
    — different SDK messages have different shapes and the redactor must
    not raise. The replacement token is ``[REDACTED:<label>]`` (or
    ``[REDACTED:<label>:<digest>]`` when ``salt`` is configured and the
    redacted value is a string; non-string values yield the bare token
    because the digest is defined only over UTF-8 cleartext).
    """

    __slots__ = ("_paths", "_salt", "_label")

    def __init__(
        self,
        paths: Sequence[str],
        *,
        salt: bytes | str | None = None,
        label: str = "field",
    ) -> None:
        if not isinstance(label, str) or not label:
            raise ValueError("FieldPathRedactor label must be a non-empty string")
        compiled: list[tuple[str, ...]] = []
        for path in paths:
            if not isinstance(path, str) or not path:
                raise ValueError(
                    "FieldPathRedactor paths must be non-empty strings"
                )
            segments = path.split(".")
            for seg in segments:
                if not seg:
                    raise ValueError(
                        f"FieldPathRedactor path {path!r} contains an empty segment"
                    )
            compiled.append(tuple(segments))
        self._paths: tuple[tuple[str, ...], ...] = tuple(compiled)
        self._salt: bytes | None = _coerce_salt(salt)
        self._label: str = label

    def redact(self, record: AuditRecord) -> AuditRecord:
        new_payload = _deep_copy(record.payload)
        for segments in self._paths:
            self._apply(new_payload, segments)
        return dataclasses.replace(record, payload=new_payload)

    def _apply(self, node: Any, segments: tuple[str, ...]) -> None:
        if not segments:
            return
        seg = segments[0]
        remaining = segments[1:]
        if seg == "*":
            if not isinstance(node, list):
                return
            for index, item in enumerate(node):
                if not remaining:
                    node[index] = self._replacement(item)
                else:
                    self._apply(item, remaining)
            return
        if not isinstance(node, dict):
            return
        if seg not in node:
            return
        if not remaining:
            node[seg] = self._replacement(node[seg])
        else:
            self._apply(node[seg], remaining)

    def _replacement(self, value: Any) -> str:
        cleartext = value if isinstance(value, str) else None
        return _format_token(self._label, cleartext, self._salt)


# --- ToolDenylistRedactor ---------------------------------------------------


class ToolDenylistRedactor:
    """Redacts tool input/result data for tools on a configured denylist.

    Only applies to :class:`SdkMessageRecord`; an :class:`EventRecord`
    passes through with a fresh-but-equal payload container. Walks the
    payload tree and, for each dict node whose ``"name"`` field is in
    the denylist, replaces sibling ``"input"`` and ``"content"`` values
    with redaction tokens. This matches the on-the-wire shape of
    ``claude-agent-sdk`` ``ToolUseBlock`` / ``ToolResultBlock`` content
    blocks (the dataclass-derived payload mapping has ``name``,
    ``input``, ``content`` as top-level keys per block).

    The envelope — record type, ``run_id``, ``id``, ``sequence``,
    ``ts``, ``attempt_number``, ``iteration_number``, ``message_type``
    — is unchanged (FR-6). The denylisted tool's *name* is intentionally
    preserved in the payload so a reader can still tell which tool was
    invoked; only its inputs and outputs are suppressed.
    """

    __slots__ = ("_names", "_salt", "_label")

    def __init__(
        self,
        tool_names: Sequence[str],
        *,
        salt: bytes | str | None = None,
        label: str = "tool",
    ) -> None:
        if not isinstance(label, str) or not label:
            raise ValueError(
                "ToolDenylistRedactor label must be a non-empty string"
            )
        names: list[str] = []
        for name in tool_names:
            if not isinstance(name, str) or not name:
                raise ValueError(
                    "ToolDenylistRedactor tool_names must be non-empty strings"
                )
            names.append(name)
        self._names: frozenset[str] = frozenset(names)
        self._salt: bytes | None = _coerce_salt(salt)
        self._label: str = label

    def redact(self, record: AuditRecord) -> AuditRecord:
        new_payload = _deep_copy(record.payload)
        if isinstance(record, SdkMessageRecord):
            self._walk(new_payload)
        return dataclasses.replace(record, payload=new_payload)

    def _walk(self, node: Any) -> None:
        if isinstance(node, dict):
            tool_name = node.get("name")
            if isinstance(tool_name, str) and tool_name in self._names:
                token_label = f"{self._label}:{tool_name}"
                if "input" in node:
                    node["input"] = _replace_value(
                        node["input"], token_label, self._salt
                    )
                if "content" in node:
                    node["content"] = _replace_value(
                        node["content"], token_label, self._salt
                    )
            for value in node.values():
                if isinstance(value, (dict, list)):
                    self._walk(value)
        elif isinstance(node, list):
            for item in node:
                if isinstance(item, (dict, list)):
                    self._walk(item)


def _replace_value(value: Any, label: str, salt: bytes | None) -> str:
    """Render a redaction token for an arbitrary payload value.

    Strings produce a digest under ``salt``; non-string values yield the
    bare ``[REDACTED:<label>]`` token because the digest is defined only
    over UTF-8 cleartext.
    """

    cleartext = value if isinstance(value, str) else None
    return _format_token(label, cleartext, salt)


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


# --- default_policy ---------------------------------------------------------


# Pattern set baked into :func:`default_policy`. Order is significant:
# specific labels (anthropic, github, pem, jwt, aws_access_key_id,
# aws_secret_access_key) run before the catch-all ``generic_api_key`` so a
# more meaningful label is preserved in the redacted output. Each entry is
# a ``(label, regex)`` pair.
_DEFAULT_PATTERNS: tuple[tuple[str, str], ...] = (
    # Anthropic API keys. ``sk-ant-...`` prefix; the body is base64url-ish
    # so allow letters, digits, ``-`` and ``_``. Length floor keeps a stray
    # "sk-ant-X" log breadcrumb from being treated as a secret.
    ("anthropic_key", r"sk-ant-[A-Za-z0-9_\-]{16,}"),
    # PEM private-key blocks across newlines. Covers ``PRIVATE KEY``,
    # ``RSA PRIVATE KEY``, ``EC PRIVATE KEY``, ``OPENSSH PRIVATE KEY``,
    # ``DSA PRIVATE KEY``, etc. Non-greedy body so two blocks in one string
    # are redacted separately.
    (
        "pem_private_key",
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    ),
    # JSON Web Tokens. Three base64url segments separated by ``.``; the
    # first two start with ``eyJ`` (the base64 of ``{"``). Length floors
    # avoid matching trivially short look-alikes.
    (
        "jwt",
        r"eyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
    ),
    # GitHub tokens: personal access (ghp_), OAuth (gho_), server (ghs_),
    # user-to-server (ghu_), refresh (ghr_). The trailing body is 20+ url-
    # safe chars; GitHub's modern format is ~36 but we accept a shorter
    # floor so stripped fixtures still match.
    ("github_token", r"gh[pousr]_[A-Za-z0-9]{20,}"),
    # AWS access key IDs (long-term ``AKIA`` and short-term ``ASIA``).
    # 16 base32-ish chars after the prefix; word boundaries keep us from
    # eating surrounding characters.
    ("aws_access_key_id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
    # AWS secret access keys. Without context a bare 40-char base64-ish
    # blob would be ambiguous, so anchor on the literal key name (case
    # insensitive) followed by ``:`` or ``=`` and the value.
    (
        "aws_secret_access_key",
        (
            r"(?i)aws_secret_access_key[\"']?\s*[:=]\s*[\"']?"
            r"[A-Za-z0-9/+=]{40}[\"']?"
        ),
    ),
    # Bearer-token headers. Anchor on the literal ``Bearer`` (or
    # ``bearer``) keyword so we redact tokens, not the user's prose.
    ("bearer_token", r"\b[Bb]earer\s+[A-Za-z0-9._\-/+=]{16,}"),
    # Generic ``sk-...`` API keys (OpenAI etc). Runs AFTER ``anthropic_key``
    # so ``sk-ant-...`` keeps the specific label. Length floor keeps real
    # API keys distinguishable from prose like ``sk-default``.
    ("generic_api_key", r"\bsk-[A-Za-z0-9_\-]{16,}"),
)


def default_policy(*, salt: bytes | str | None = None) -> Redactor:
    """Return a :class:`Redactor` over the standard built-in secret patterns.

    Covers (FR-3): Anthropic API keys, generic ``sk-`` API keys, AWS access
    key IDs and contextual secret keys, PEM private-key blocks, JSON Web
    Tokens, GitHub tokens, and bearer-token headers. Best-effort — unmatched
    secrets still pass through; consumers needing custom patterns should
    layer their own redactors via :func:`compose`.

    Supplying ``salt`` switches every token to the salted
    ``[REDACTED:<label>:<digest>]`` form (FR-8). Output is byte-identical
    across runs and processes for the same ``(record, salt)`` (NFR-3).
    """

    return PatternRedactor(_DEFAULT_PATTERNS, salt=salt)


__all__ = [
    "EnvValueRedactor",
    "FieldPathRedactor",
    "PatternRedactor",
    "Redactor",
    "ToolDenylistRedactor",
    "compose",
    "default_policy",
]

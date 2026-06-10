"""Command-line surface for ``python -m flywheel_core.audit``.

Thin wrapper over :func:`flywheel_core.audit.stream` that handles argument
parsing, store construction, and two output formats:

* **Default** -- one record per line, formatted as::

      <iso-ts> seq=<n> attempt=<a> iter=<i> kind=<k> | <preview>

  where ``<preview>`` is a short single-line summary capped at
  :data:`PREVIEW_MAX_CHARS` characters. Longer payloads are truncated
  and a hint pointing to ``--json`` is appended so the reader can fetch
  the full payload on demand.

* **NDJSON** (``--json``) -- one JSON object per line; every record
  round-trips through :func:`json.loads` losslessly. Non-JSON-native
  values (``datetime``, ``bytes``) are coerced via :func:`_json_default`.

Redaction is **on by default** (FR-10): every record is passed through a
best-effort :class:`Redactor` before display, and a single-line stderr
notice confirms the policy. ``--raw`` opts back into verbatim output for
authorized forensics. ``--redact-policy`` selects a named built-in
(``default`` / ``strict``) or loads a custom ``Redactor``-returning
callable from a dotted path of the form ``module.sub:factory``.
``--redact-salt`` switches every token to the salted
``[REDACTED:<label>:<digest>]`` form (HMAC-SHA256) so identical secrets
correlate across records without ever being recoverable. ``--dry-run``
streams the run through the selected policy and emits a coverage report
(records scanned, records redacted, hits per label) without printing any
payload content (FR-12).

The CLI is intentionally minimal -- the spec defers ``--kind``,
``--attempt``, ``--summary`` flags to a follow-up feature.
"""

from __future__ import annotations

import argparse
import base64
import importlib
import json
import os
import re
import sys
from collections.abc import Iterable, Iterator, Mapping
from datetime import datetime
from pathlib import Path
from typing import Any, TextIO

from flywheel_core.audit import stream
from flywheel_core.redaction import (
    EnvValueRedactor,
    Redactor,
    compose,
    default_policy,
    strict_policy,
)
from flywheel_core.store_protocols import (
    AuditRecord,
    EventRecord,
    SdkMessageRecord,
)
from flywheel_core.store_sqlite import SqliteStore


# Default store path used when neither ``--db`` nor ``$FLYWHEEL_DB`` is
# set. Matches the spec's "./flywheel.db" fallback rather than
# workflow.py's ".flywheel/flywheel.sqlite" -- the audit CLI is a
# debugging tool that may run outside a ``.flywheel/`` layout.
DEFAULT_DB_PATH: Path = Path("flywheel.db")
DEFAULT_POLL_INTERVAL: float = 0.25

# Total preview budget for the default human-readable mode. Fixed (not
# terminal-width-derived) so output is reproducible across pipes and
# tests; the spec calls out the ~80-char tail explicitly.
PREVIEW_MAX_CHARS: int = 80

# Truncation hint appended when the preview body would exceed
# :data:`PREVIEW_MAX_CHARS`. The hint occupies part of the budget so
# the final line never exceeds it.
TRUNCATION_HINT: str = "... (use --json for full payload)"

# Environment variables whose ambient values the CLI seeds into an
# :class:`EnvValueRedactor` when building a named built-in policy. Only
# ``ANTHROPIC_API_KEY`` is captured -- the redactor itself silently drops
# absent or too-short values, so this is safe to extend later without
# breaking callers.
_CLI_ENV_SEEDS: tuple[str, ...] = ("ANTHROPIC_API_KEY",)

# Regex used by ``--dry-run`` to discover ``[REDACTED:<label>[:<digest>]]``
# tokens inside the redacted payload tree. The inner capture stops at the
# closing ``]`` so payloads that contain other bracketed text are matched
# correctly. Labels may legitimately contain colons (e.g. ``tool:Read``);
# digest stripping is handled in :func:`_strip_digest`.
_REDACTION_TOKEN_RE = re.compile(r"\[REDACTED:([^\[\]]+)\]")

# Width of the salted digest suffix produced by
# :func:`flywheel_core.redaction._format_token`. Kept in sync with
# ``flywheel_core.redaction._DIGEST_HEX_LEN``; mismatches would only cause
# ``--dry-run`` to mis-attribute labels, never cause a redaction bug.
_DIGEST_HEX_LEN: int = 8

# Names of the built-in named policies addressable via
# ``--redact-policy <name>``. Anything else is treated as a dotted path of
# the form ``module.sub:factory`` (FR-11).
_BUILTIN_POLICY_NAMES: frozenset[str] = frozenset({"default", "strict"})


def _resolve_db(arg: str | None) -> Path:
    """Resolve the ``--db`` argument with ``$FLYWHEEL_DB`` fallback.

    Precedence: explicit ``--db`` > ``$FLYWHEEL_DB`` env var >
    :data:`DEFAULT_DB_PATH`. The empty-string env var is treated as
    unset so a stray ``FLYWHEEL_DB=`` does not silently shadow the
    default.
    """
    if arg:
        return Path(arg)
    env = os.environ.get("FLYWHEEL_DB")
    if env:
        return Path(env)
    return DEFAULT_DB_PATH


def _json_default(value: Any) -> Any:
    """Fallback encoder for objects ``json.dumps`` cannot handle.

    The library already persists payloads as JSON-compatible dicts so
    this is rarely hit; the spec calls for defensive coverage of
    ``datetime`` (ISO-8601) and ``bytes`` (base64) shapes. Any other
    non-native type raises :class:`TypeError` so we never silently emit
    truncated or stringified data.
    """
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, (bytes, bytearray)):
        return base64.b64encode(bytes(value)).decode("ascii")
    raise TypeError(f"cannot JSON-serialize {type(value).__name__}")


def _record_kind(record: AuditRecord) -> str:
    """Return the ``kind=<k>`` token for the default-mode line.

    Mirrors the convention used by the logger emitter
    (``event:<kind>`` / ``sdk:<message_type>``) so default-mode output
    reads the same way as :func:`flywheel_core.audit.attach_logger` log
    messages.
    """
    if isinstance(record, EventRecord):
        return f"event:{record.kind}"
    return f"sdk:{record.message_type}"


def _record_attempt(record: AuditRecord) -> str:
    """Stringify ``attempt_number`` with ``"-"`` for ``None``.

    ``EventRecord.attempt_number`` is optional; we render the empty
    case as ``"-"`` so default-mode columns line up regardless of
    record type.
    """
    if isinstance(record, EventRecord):
        if record.attempt_number is None:
            return "-"
        return str(record.attempt_number)
    return str(record.attempt_number)


def _record_iter(record: AuditRecord) -> str:
    """Stringify ``iteration_number`` with ``"-"`` for events.

    Events have no iteration index by construction (a single attempt
    spans many iterations); rendering them as ``"-"`` keeps the
    column-aligned look without lying about which records carry an
    iteration.
    """
    if isinstance(record, SdkMessageRecord):
        return str(record.iteration_number)
    return "-"


def _record_iter_value(record: AuditRecord) -> int | None:
    """Return the JSON-mode ``iteration_number`` (``None`` for events)."""
    if isinstance(record, SdkMessageRecord):
        return record.iteration_number
    return None


def _payload_preview(record: AuditRecord) -> str:
    """Return a single-line truncated payload preview.

    The payload is rendered as compact JSON (sorted keys for
    determinism), embedded newlines flattened to spaces. If the result
    is longer than :data:`PREVIEW_MAX_CHARS` we replace the tail with
    :data:`TRUNCATION_HINT` so the total stays within budget.
    """
    # Both record arms carry a ``Mapping[str, Any]`` ``payload`` so the
    # rendering is identical regardless of type; the per-type discriminator
    # already lives in ``kind=<k>``.
    payload: Mapping[str, Any] = record.payload
    body = json.dumps(
        dict(payload), default=_json_default, sort_keys=True, separators=(",", ":")
    )
    body = body.replace("\n", " ").replace("\r", " ")
    if len(body) <= PREVIEW_MAX_CHARS:
        return body
    keep = PREVIEW_MAX_CHARS - len(TRUNCATION_HINT)
    if keep < 0:
        keep = 0
    return body[:keep] + TRUNCATION_HINT


def _render_default(record: AuditRecord) -> str:
    """Format one record as a default-mode line.

    Format: ``<iso-ts> seq=<n> attempt=<a> iter=<i> kind=<k> | <preview>``.
    Sequence is rendered as ``"?"`` if absent (the store contract says
    it never will be, but we don't crash on the defensive case).
    """
    ts = record.ts.isoformat()
    seq = "?" if record.sequence is None else str(record.sequence)
    attempt = _record_attempt(record)
    iter_str = _record_iter(record)
    kind = _record_kind(record)
    preview = _payload_preview(record)
    return f"{ts} seq={seq} attempt={attempt} iter={iter_str} kind={kind} | {preview}"


def _render_json(record: AuditRecord) -> str:
    """Format one record as an NDJSON object.

    The schema is fixed so downstream consumers can rely on the field
    set without sniffing the record type: ``iteration_number`` is
    ``null`` for events, ``kind_or_message_type`` carries the per-type
    discriminator without a prefix (callers can keep their own
    isinstance-style switch).
    """
    obj: dict[str, Any] = {
        "ts": record.ts.isoformat(),
        "sequence": record.sequence,
        "run_id": record.run_id,
        "attempt_number": record.attempt_number,
        "iteration_number": _record_iter_value(record),
        "kind_or_message_type": (
            record.kind
            if isinstance(record, EventRecord)
            else record.message_type
        ),
        "payload": dict(record.payload),
    }
    return json.dumps(obj, default=_json_default, sort_keys=True)


# --- Redaction wiring -------------------------------------------------------


class _PolicyError(ValueError):
    """Raised when ``--redact-policy`` resolution fails.

    A distinct subclass lets :func:`main` translate any policy resolution
    failure into a single ``returncode=2`` exit with a clear stderr
    message, instead of letting an :class:`ImportError` /
    :class:`AttributeError` / :class:`TypeError` bubble up as an
    untranslated Python traceback (FR-11: "fail fast with a clear error
    before streaming").
    """


def _build_named_policy(name: str, *, salt: bytes | None) -> Redactor:
    """Build one of the built-in named policies (``default`` or ``strict``).

    Both named policies are composed with an
    :class:`EnvValueRedactor` seeded from the ambient values of
    :data:`_CLI_ENV_SEEDS` so the operator's own API keys are redacted
    even when they appear verbatim in agent output (the most common
    leak vector). ``EnvValueRedactor`` itself silently drops absent or
    too-short values, so the composition is safe regardless of which
    env vars are actually set.
    """

    if name == "default":
        base: Redactor = default_policy(salt=salt)
    elif name == "strict":
        base = strict_policy(salt=salt)
    else:  # pragma: no cover - guarded by caller
        raise _PolicyError(f"unknown built-in policy {name!r}")
    env_redactor = EnvValueRedactor(list(_CLI_ENV_SEEDS), salt=salt)
    return compose(base, env_redactor)


def _resolve_policy(
    name_or_path: str | None,
    *,
    salt: bytes | None,
) -> tuple[Redactor, str]:
    """Resolve the requested ``--redact-policy`` value.

    Returns a ``(redactor, display_label)`` pair. ``name_or_path`` may be:

    * ``None`` -- equivalent to ``"default"``; the CLI's redact-by-default
      behavior.
    * ``"default"`` / ``"strict"`` -- built-in named policies (FR-11).
    * ``"module.sub:factory"`` -- dotted path to a ``Redactor``-returning
      callable in any importable module. Resolved with
      :func:`importlib.import_module`; the salt is **not** auto-applied
      because the custom factory owns its own configuration.

    Resolution failures (no such module/attribute, wrong return type,
    bad regex inside the factory) raise :class:`_PolicyError` so
    :func:`main` can translate them into a single ``returncode=2`` exit
    before any record is streamed.
    """

    if name_or_path is None or name_or_path in _BUILTIN_POLICY_NAMES:
        builtin = name_or_path or "default"
        return _build_named_policy(builtin, salt=salt), builtin

    if ":" not in name_or_path:
        raise _PolicyError(
            f"unknown built-in policy {name_or_path!r}; expected one of "
            f"{sorted(_BUILTIN_POLICY_NAMES)} or a dotted path "
            f"'module.sub:factory'"
        )

    module_path, _, attr = name_or_path.partition(":")
    if not module_path or not attr:
        raise _PolicyError(
            f"invalid dotted path {name_or_path!r}; expected 'module.sub:factory'"
        )

    try:
        module = importlib.import_module(module_path)
    except ImportError as exc:
        raise _PolicyError(
            f"cannot import module {module_path!r} for --redact-policy "
            f"{name_or_path!r}: {exc}"
        ) from exc

    try:
        factory = getattr(module, attr)
    except AttributeError as exc:
        raise _PolicyError(
            f"module {module_path!r} has no attribute {attr!r} for "
            f"--redact-policy {name_or_path!r}"
        ) from exc

    if not callable(factory):
        raise _PolicyError(
            f"--redact-policy {name_or_path!r} resolved to "
            f"{type(factory).__name__}, not a callable"
        )

    try:
        redactor = factory()
    except Exception as exc:
        # Includes the FR-11 "malformed regex in a custom dotted-path
        # policy" case: the factory raises during construction (e.g.
        # ``re.error`` re-raised as ``ValueError`` by ``PatternRedactor``)
        # and we surface it before any record is streamed.
        raise _PolicyError(
            f"--redact-policy {name_or_path!r} factory raised: "
            f"{type(exc).__name__}: {exc}"
        ) from exc

    if not isinstance(redactor, Redactor):
        raise _PolicyError(
            f"--redact-policy {name_or_path!r} returned "
            f"{type(redactor).__name__}, not a Redactor"
        )

    return redactor, name_or_path


def _strip_digest(label: str, salt_in_use: bool) -> str:
    """Strip the trailing ``:<digest>`` suffix from a token label when salt
    is in use.

    Bare tokens look like ``[REDACTED:anthropic_key]``; salted tokens look
    like ``[REDACTED:anthropic_key:9f2a1b3c]``. Labels may legitimately
    contain colons (``tool:Read``), so we only strip the last segment and
    only when it is exactly :data:`_DIGEST_HEX_LEN` hex chars.
    """

    if not salt_in_use:
        return label
    head, sep, tail = label.rpartition(":")
    if not sep or len(tail) != _DIGEST_HEX_LEN:
        return label
    if not all(c in "0123456789abcdef" for c in tail):
        return label
    return head


def _count_tokens(
    value: Any,
    label_counts: dict[str, int],
    *,
    salt_in_use: bool,
) -> int:
    """Walk ``value`` recursively, count every ``[REDACTED:label[:digest]]``
    token by stripped label, and return the total count.

    Used by ``--dry-run`` to produce per-label hit counts (FR-12) without
    re-running the redactor: the redactor has already emitted the tokens
    and the labels are part of the wire format.
    """

    if isinstance(value, str):
        total = 0
        for match in _REDACTION_TOKEN_RE.finditer(value):
            label = _strip_digest(match.group(1), salt_in_use)
            label_counts[label] = label_counts.get(label, 0) + 1
            total += 1
        return total
    if isinstance(value, Mapping):
        return sum(
            _count_tokens(item, label_counts, salt_in_use=salt_in_use)
            for item in value.values()
        )
    if isinstance(value, (list, tuple)):
        return sum(
            _count_tokens(item, label_counts, salt_in_use=salt_in_use)
            for item in value
        )
    return 0


def _emit_dry_run_summary(
    out: TextIO,
    *,
    policy_label: str,
    records_scanned: int,
    records_redacted: int,
    total_redactions: int,
    label_counts: Mapping[str, int],
) -> None:
    """Print the ``--dry-run`` coverage report to ``out``.

    The report intentionally emits **no payload content** (FR-12): only
    aggregate counts and per-label hit totals. Sorted by ``(count desc,
    label asc)`` so high-traffic labels are visible first and the output
    is byte-deterministic across runs (NFR-3).
    """

    print("dry-run coverage report", file=out)
    print(f"policy: {policy_label}", file=out)
    print(f"records scanned: {records_scanned}", file=out)
    print(f"records redacted: {records_redacted}", file=out)
    print(f"total redactions: {total_redactions}", file=out)
    if not label_counts:
        print("hits by label: (none)", file=out)
        return
    print("hits by label:", file=out)
    for label, count in sorted(label_counts.items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"  {label}: {count}", file=out)


def _emit_redaction_notice(err: TextIO, *, policy_label: str) -> None:
    """Print the one-line stderr notice required by FR-10 + NFR-1.

    Two lines: the literal FR-10 sentence (parameterized by the active
    policy label so ``strict`` and custom policies are self-describing)
    followed by the NFR-1 best-effort caveat so the operator can never
    mistake "redacted" for "safe".
    """

    print(
        f"redaction: {policy_label} policy applied (use --raw for verbatim)",
        file=err,
    )
    print(
        "note: redaction is best-effort; unmatched secrets may pass through; "
        "the store remains the unredacted source of truth",
        file=err,
    )


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m flywheel_core.audit",
        description=(
            "Print the totally-ordered audit stream for one run. Default "
            "output is a human-readable one-line-per-record view; --json "
            "switches to NDJSON; --follow tails until the lifecycle "
            "reaches a terminal status (Ctrl-C exits cleanly with code "
            "130).\n"
            "\n"
            "Redaction is on by default: payloads pass through a "
            "best-effort policy before display so the output is safe to "
            "paste into tickets or chats. Use --raw to opt out for "
            "authorized forensics. Redaction is best-effort -- unmatched "
            "secrets can still pass through, and the store remains the "
            "unredacted source of truth."
        ),
        epilog=(
            "Redaction usage (on by default; best-effort -- the store "
            "remains the unredacted source of truth):\n"
            "  --redact                   explicit opt-in (already the default)\n"
            "  --redact-policy NAME|PATH  'default' | 'strict' | "
            "'module.sub:factory'\n"
            "  --redact-salt SALT         stable [REDACTED:label:digest] "
            "tokens for correlation\n"
            "  --dry-run                  coverage report only; no payload "
            "content printed\n"
            "  --raw                      verbatim output for authorized "
            "forensics (disables redaction)\n"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "run_id",
        help="The run_id whose audit stream to read.",
    )
    parser.add_argument(
        "--db",
        default=None,
        help=(
            f"SQLite database path (default: $FLYWHEEL_DB or "
            f"{DEFAULT_DB_PATH})."
        ),
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit one JSON object per record (NDJSON) instead of the text view.",
    )
    parser.add_argument(
        "--follow",
        action="store_true",
        help=(
            "Tail the audit stream live; exits when the lifecycle "
            "reaches a terminal status (DONE, FAILED, INTERNAL_ERROR, "
            "INTERRUPTED)."
        ),
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=DEFAULT_POLL_INTERVAL,
        help=(
            f"Polling interval for --follow in seconds "
            f"(default: {DEFAULT_POLL_INTERVAL})."
        ),
    )
    parser.add_argument(
        "--redact",
        action="store_true",
        help=(
            "Explicitly enable redaction (already on by default). Useful "
            "in scripts that want unambiguous intent."
        ),
    )
    parser.add_argument(
        "--redact-policy",
        default=None,
        metavar="NAME-OR-DOTTED-PATH",
        help=(
            "Select a redaction policy. Built-in names: 'default' "
            "(pattern set + env-value redactor seeded from "
            "ANTHROPIC_API_KEY), 'strict' (default + denylist for "
            "credential-reading tools Read/Bash). A dotted path of the "
            "form 'module.sub:factory' resolves a Redactor-returning "
            "callable in any importable module; resolution failures "
            "fail fast before streaming."
        ),
    )
    parser.add_argument(
        "--raw",
        action="store_true",
        help=(
            "Disable redaction and emit verbatim payloads exactly as "
            "stored. Use only for authorized forensics on trusted "
            "output sinks."
        ),
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help=(
            "Stream the run through the selected policy and emit a "
            "coverage report (records scanned, records redacted, hits "
            "per label) without printing any payload content. Useful "
            "for verifying a custom policy catches the secrets you "
            "expect before relying on it."
        ),
    )
    parser.add_argument(
        "--redact-salt",
        default=None,
        metavar="SALT",
        help=(
            "Salt for stable [REDACTED:label:digest] tokens "
            "(HMAC-SHA256). Identical secrets render to the same token "
            "across records so a consumer can correlate occurrences; "
            "the cleartext is never recoverable from a token. Default: "
            "no salt -> bare [REDACTED:label] tokens. Applies to "
            "built-in policies only -- custom dotted-path factories "
            "configure their own salt."
        ),
    )
    return parser


def _validate_flag_combinations(args: argparse.Namespace) -> str | None:
    """Return a human-readable error string for any disallowed flag
    combination, or ``None`` if the combination is consistent.

    ``--raw`` is the only mutually-exclusive flag: combining it with any
    redaction-shaping flag is operator confusion (do you want redacted or
    verbatim?). All other combinations are valid -- ``--redact`` is a
    no-op next to ``--redact-policy``, ``--dry-run`` implies redaction is
    on, and ``--redact-salt`` is meaningful for any selected policy.
    """

    if args.raw:
        conflicting: list[str] = []
        if args.redact:
            conflicting.append("--redact")
        if args.redact_policy is not None:
            conflicting.append("--redact-policy")
        if args.dry_run:
            conflicting.append("--dry-run")
        if args.redact_salt is not None:
            conflicting.append("--redact-salt")
        if conflicting:
            return (
                "--raw cannot be combined with "
                + ", ".join(conflicting)
                + " (redaction is either fully off or fully on)"
            )
    return None


def main(
    argv: Iterable[str] | None = None,
    *,
    stdout: TextIO | None = None,
    stderr: TextIO | None = None,
) -> int:
    """Entry point for ``python -m flywheel_core.audit``.

    Returns the process exit code. ``stdout``/``stderr`` are injectable
    so unit tests can capture output without subprocess overhead; both
    default to the real streams.
    """
    parser = _build_parser()
    args = parser.parse_args(list(argv) if argv is not None else None)
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr

    flag_error = _validate_flag_combinations(args)
    if flag_error is not None:
        print(f"error: {flag_error}", file=err)
        return 2

    # Resolve the policy *before* opening the store so a malformed
    # policy fails fast with a clean exit, not midway through a stream
    # (FR-11). ``--raw`` skips this entirely and streams verbatim.
    redactor: Redactor | None
    policy_label: str
    if args.raw:
        redactor = None
        policy_label = "raw"
    else:
        salt_bytes = (
            args.redact_salt.encode("utf-8")
            if args.redact_salt is not None
            else None
        )
        try:
            redactor, policy_label = _resolve_policy(
                args.redact_policy, salt=salt_bytes
            )
        except _PolicyError as exc:
            print(f"error: {exc}", file=err)
            return 2

    db_path = _resolve_db(args.db)
    store = SqliteStore(db_path)
    try:
        # Spec error-handling table: unknown run_id yields nothing and
        # exits 0 with a clear stderr message. We check the lifecycle
        # *before* draining so the message fires whether or not any
        # records exist (an orphaned event without a lifecycle row is a
        # store-integrity bug, not a user-facing error). The redaction
        # notice deliberately fires only on the streaming path so the
        # "no records" branch stays a single-line stderr.
        if store.load_lifecycle(args.run_id) is None:
            print(f"no records for run_id {args.run_id}", file=err)
            return 0

        if redactor is not None:
            _emit_redaction_notice(err, policy_label=policy_label)

        iterator: Iterator[AuditRecord] = stream(
            args.run_id,
            store=store,
            follow=args.follow,
            poll_interval=args.poll_interval,
        )
        try:
            if args.dry_run:
                # Dry-run uses the redactor but suppresses payload
                # output entirely (FR-12). ``redactor`` is guaranteed
                # non-None here because --dry-run cannot combine with
                # --raw (see _validate_flag_combinations).
                assert redactor is not None
                _drive_dry_run(
                    iterator,
                    out=out,
                    redactor=redactor,
                    policy_label=policy_label,
                    salt_in_use=args.redact_salt is not None,
                )
            else:
                render = _render_json if args.json else _render_default
                for record in iterator:
                    if redactor is not None:
                        record = redactor.redact(record)
                    print(render(record), file=out)
                    out.flush()
        except KeyboardInterrupt:
            # SIGINT under --follow: exit cleanly per the spec without
            # a Python stack trace. Code 130 matches the conventional
            # "terminated by Ctrl-C" exit; documented in --help.
            return 130
    finally:
        store.close()
    return 0


def _drive_dry_run(
    iterator: Iterator[AuditRecord],
    *,
    out: TextIO,
    redactor: Redactor,
    policy_label: str,
    salt_in_use: bool,
) -> None:
    """Consume the audit stream under ``--dry-run`` semantics.

    Each record is passed through ``redactor`` and inspected; the
    aggregate report is emitted once after the iterator drains. No
    record payload is ever written to ``out`` (FR-12).
    """

    records_scanned = 0
    records_redacted = 0
    total_redactions = 0
    label_counts: dict[str, int] = {}
    for record in iterator:
        records_scanned += 1
        redacted = redactor.redact(record)
        record_hits = _count_tokens(
            redacted.payload, label_counts, salt_in_use=salt_in_use
        )
        # ``record_hits > 0`` is sufficient to know at least one
        # redaction fired on this record; comparing the payloads would
        # also catch ``FieldPathRedactor`` blanking a non-string field,
        # but the FieldPathRedactor still emits a ``[REDACTED:<label>]``
        # token so token-counting is in fact complete.
        if record_hits > 0:
            records_redacted += 1
            total_redactions += record_hits
    _emit_dry_run_summary(
        out,
        policy_label=policy_label,
        records_scanned=records_scanned,
        records_redacted=records_redacted,
        total_redactions=total_redactions,
        label_counts=label_counts,
    )


__all__ = [
    "DEFAULT_DB_PATH",
    "DEFAULT_POLL_INTERVAL",
    "PREVIEW_MAX_CHARS",
    "TRUNCATION_HINT",
    "main",
]

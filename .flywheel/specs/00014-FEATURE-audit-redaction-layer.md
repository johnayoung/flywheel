# Feature: Audit redaction layer

## Summary

A read-time redaction layer that sits on top of the audit stream. The store
stays sensitive-by-default — payloads are still captured verbatim — but
`flywheel.audit.stream`, `subscribe`, `attach_logger`, and the
`python -m flywheel.audit` CLI gain an optional `Redactor` that transforms
each `AuditRecord` as it is read, replacing secrets (API keys, tokens, private
keys, known sensitive env-var values) and operator-nominated payload fields
with stable placeholder tokens. This makes audit data safe to share, paste
into tickets, ship to log sinks, and screen-share without mutating or
destroying the durable forensic record.

## Background

`docs/vision.md` (the "Audit stream" section, line 46) states the store is
"sensitive-by-default: payloads are captured verbatim with no truncation or
redaction, so audit records may contain prompts, tool inputs, and tool outputs
in the clear. A redaction layer is future work that will sit on top of this
stream." Spec `00006-FEATURE-audit-stream.md` listed redaction as an explicit
non-goal: "A redaction layer is a future feature on top of this one."

Today every consumer of the audit surface sees raw bytes:

- `flywheel.audit.stream` / `subscribe` / `attach_logger`
  (`src/flywheel/audit/__init__.py`) yield the store's
  `EventRecord` / `SdkMessageRecord` dataclasses unchanged.
- `python -m flywheel.audit` (`src/flywheel/audit/_cli.py`) renders previews
  and `--json` NDJSON straight from those payloads.

`SdkMessageRecord.payload` carries tool inputs and outputs verbatim — these
routinely contain `ANTHROPIC_API_KEY`, cloud credentials, `.env` contents,
and private keys that the agent read or echoed. `EventRecord.payload` carries
prompt text and sandbox/environment context. There is currently no way to
produce a shareable view of a run short of hand-editing JSON. This feature
closes that gap without weakening the store's role as the authoritative,
complete forensic record.

## Scope

### In Scope

- New pure module `src/flywheel/redaction.py` exposing a `Redactor` protocol,
  a data-driven `RedactionPolicy`, a set of built-in redactors
  (pattern / env-value / field-path / tool-denylist), a `compose` combinator,
  and a `default_policy()` factory of common secret patterns.
- Redaction operates on `AuditRecord` (`EventRecord` | `SdkMessageRecord`),
  returning a new record of the same type with a redacted `payload` mapping.
  It is a pure function of `(record, policy)` — no I/O, no mutation of the
  input.
- **Wiring-agnostic abstraction.** The `Redactor` operates on a record in
  isolation so the *same* object can be applied at read time (this MVP) or at
  the persistence seam (opt-in follow-up — see "Designed-for extension"
  below). The MVP ships read-time wiring; nothing in the abstraction couples
  it to the read path.
- Pluggable policy loading for the CLI: `--redact-policy <dotted.path>` resolves
  a `Redactor`-returning callable in any importable module (e.g.
  `mypkg.audit:corp_policy`), so consumers configure their own policy without
  forking flywheel. Built-in policies are addressable by short name.
- A `--dry-run` / coverage mode on the CLI that streams a run through a policy
  and reports per-record and per-pattern hit counts (no payloads emitted), so a
  consumer can verify their policy catches their secrets before trusting it.
- Stable placeholder tokens: a matched secret is replaced by
  `[REDACTED:<label>]` or, when correlation is desired,
  `[REDACTED:<label>:<digest>]` where `<digest>` is a short HMAC-SHA256 prefix
  over the cleartext keyed by a caller-supplied salt, so the same secret maps
  to the same token across records without exposing it.
- Optional `redactor: Redactor | None = None` parameter threaded through
  `flywheel.audit.stream`, `subscribe`, and `attach_logger`. Default `None`
  preserves today's verbatim behavior for programmatic callers.
- CLI flags on `python -m flywheel.audit`: `--redact` (apply
  `default_policy()`), `--redact-policy <name>` (named built-in policies), and
  `--raw` (force verbatim). The CLI redacts by default and prints a one-line
  stderr notice when it does; `--raw` opts out for authorized forensics.
- Deep traversal of nested payload mappings/sequences: redaction reaches into
  arbitrarily nested dicts and lists inside a payload, not just top-level keys.
- Documentation: update the "Audit stream" section of `docs/vision.md` to
  point at the redaction layer, and a short usage section in the audit CLI
  `--help` text.

### Out of Scope

- *Building* write-time redaction in this feature. The store remains verbatim
  and sensitive-by-default and this MVP wires redaction only at read time.
  However, the `Redactor` abstraction is explicitly designed so a write-time
  opt-in is a clean follow-up, not a redesign — see "Designed-for extension:
  write-time redaction" under Technical Context. The non-goal is the
  *implementation*, not the *seam*.
- Schema changes, migrations, or re-writing already-stored bytes. No store
  backend (`InMemoryStore`, `SqliteStore`, `PostgresStore`) is touched.
- Encryption at rest, key escrow, or any reversible/decryptable redaction.
  Redaction is one-way by design.
- ML/heuristic PII classification (names, addresses, free-text PII). The MVP
  redacts structured secrets, known sensitive env-var values, and
  operator-named fields only.
- Per-record access control, authz, or audit-of-the-audit (who viewed raw).
- Redacting the harness's in-process `on_message` live stdout stream or the
  `agent_output` fields on `Attempt` / `Lifecycle`.

## Requirements

### Functional Requirements

1. **FR-1: Pure redaction module.** `flywheel.redaction` imports no `json`,
   `pathlib`, `io`, or `open()` and performs no I/O. It depends only on the
   `AuditRecord` dataclasses and the standard `re` / `hmac` / `hashlib`
   modules. (Mirrors the purity discipline of `flywheel.task` /
   `flywheel.lifecycle`, enforced by an analogous purity test.)

2. **FR-2: Type-preserving transform.** `Redactor.redact(record)` returns a
   record of the same concrete type (`EventRecord` stays `EventRecord`,
   `SdkMessageRecord` stays `SdkMessageRecord`) with `sequence`, `id`, `ts`,
   `run_id`, and all non-payload fields unchanged. Only the `payload` mapping
   (and any free-text fields the policy targets) is transformed. The input
   record is never mutated.

3. **FR-3: Pattern redaction.** A `PatternRedactor` replaces substrings
   matching configured regexes within any string value found in the payload
   tree. `default_policy()` ships patterns for at least: Anthropic keys
   (`sk-ant-...`), generic `sk-...` API keys, AWS access key IDs (`AKIA...`)
   and secret keys, PEM private-key blocks (`-----BEGIN ... PRIVATE KEY-----`),
   JWTs, GitHub tokens (`ghp_` / `gho_` / `ghs_`), and bearer-token headers.

4. **FR-4: Env-value redaction.** An `EnvValueRedactor` redacts the literal
   *values* of a configured set of environment-variable names that are present
   in the current process environment (e.g. `ANTHROPIC_API_KEY`), matching
   those values wherever they appear in any payload string. Variable names are
   configured explicitly; the redactor reads `os.environ` only at construction
   time and the captured values stay in memory (not part of the pure core —
   see Design).

5. **FR-5: Field-path redaction.** A `FieldPathRedactor` redacts the entire
   value at operator-named payload key paths (e.g. `tool_input.command`,
   `env`), regardless of content, replacing it with a placeholder. Paths
   support nested keys and a wildcard for list elements.

6. **FR-6: Tool-denylist redaction.** A `ToolDenylistRedactor` redacts the
   input and result payloads of `SdkMessageRecord`s whose tool name is on a
   configured denylist (e.g. a tool that reads credential files), leaving the
   record's envelope (type, ids, sequence) intact.

7. **FR-7: Composition.** `compose(*redactors)` returns a `Redactor` that
   applies each in order; the output of one feeds the next. Order is
   deterministic and documented.

8. **FR-8: Stable tokens.** Replacement tokens are deterministic for a given
   `(cleartext, policy)`. When a salt is configured, identical cleartext yields
   an identical `[REDACTED:<label>:<digest>]` token across all records in a
   stream so consumers can correlate occurrences; without a salt the token is
   the bare `[REDACTED:<label>]`. The digest is one-way (HMAC-SHA256 prefix);
   cleartext is never recoverable from a token.

9. **FR-9: Stream integration.** `flywheel.audit.stream`, `subscribe`, and
   `attach_logger` accept `redactor: Redactor | None = None`. When provided,
   every yielded/dispatched record passes through `redactor.redact` before the
   consumer sees it; ordering, follow semantics, cursor math, and error
   isolation are unchanged. `None` is verbatim.

10. **FR-10: CLI integration.** `python -m flywheel.audit` applies
    `default_policy()` by default and prints `redaction: default policy
    applied (use --raw for verbatim)` to stderr once. `--redact-policy <name>`
    selects a named policy; `--raw` disables redaction. Redaction is applied
    identically to default and `--json` output, so NDJSON consumers get
    redacted JSON.

11. **FR-11: Pluggable policies.** `--redact-policy <name-or-dotted-path>`
    resolves either a built-in policy by short name or a `Redactor`-returning
    callable at an importable dotted path (`module.sub:factory`). Resolution
    failures (no such module/attribute, wrong return type) fail fast with a
    clear error before streaming. Consumers register custom policies without
    modifying flywheel.

12. **FR-12: Policy coverage / dry-run.** `--dry-run` streams a run through the
    selected policy and emits a coverage summary — total records scanned,
    records with at least one redaction, and per-pattern/per-redactor hit
    counts — without printing any payload content. This lets a consumer verify
    a policy catches their secret formats before relying on it.

13. **FR-13: Wiring-agnostic Redactor.** A `Redactor` is defined and tested as
    a pure transform over a single `AuditRecord`, with no reference to the read
    path, the store, or streaming. (Validates that the same object is reusable
    at the persistence seam in the follow-up without change.)

### Non-Functional Requirements

1. **NFR-1: No false sense of completeness.** Redaction is best-effort secret
   suppression, not a guarantee. The CLI notice and module docstring must state
   plainly that unmatched secrets can still pass through and the store remains
   the unredacted source of truth. No silent claim of "safe."

2. **NFR-2: Bounded cost.** Redacting a record is linear in payload size with a
   fixed pattern set; the follow loop's latency budget is unaffected. Compiled
   regexes are reused, not recompiled per record.

3. **NFR-3: Determinism.** Given the same record and policy, output is
   byte-identical across runs and processes (salt is explicit input, never
   random; no clock dependence).

4. **NFR-4: Backward compatibility.** Existing programmatic callers that do not
   pass `redactor` observe no behavior change. The CLI's new default is the
   only behavior change, and `--raw` restores prior output exactly.

## Behavior Specification

### Happy Path

1. Operator runs `python -m flywheel.audit <run_id>` on a finished run whose
   tool outputs contain `ANTHROPIC_API_KEY=sk-ant-abc123`.
2. The CLI builds `default_policy()` plus an `EnvValueRedactor` seeded from the
   ambient `ANTHROPIC_API_KEY`, composes them, and streams the run.
3. Each record passes through the composed redactor; the key value renders as
   `[REDACTED:anthropic_key]` (or `[REDACTED:anthropic_key:9f2a]` with a salt).
4. A one-line stderr notice confirms redaction is on. The operator pastes the
   output into a ticket safely.
5. A consumer needing the raw bytes for forensics re-runs with `--raw` and gets
   today's verbatim output.

### Error Handling

- A malformed regex in a custom policy fails fast at policy construction with a
  clear error, never mid-stream.
- A redactor that raises on one record must not corrupt the stream: in
  `subscribe`/`attach_logger` the existing per-record isolation applies, and in
  `stream` a redactor exception propagates to the caller (the iterator is a
  direct API, consistent with how `stream` surfaces other errors). The built-in
  redactors are written to never raise on well-formed `AuditRecord`s.
- A payload value that is not a string (int, bool, `None`, nested container) is
  traversed structurally; non-string leaves are passed through untouched.

### Edge Cases

- A secret split across two payload fields is redacted in each field
  independently; cross-field reconstruction is out of scope.
- A secret appearing inside a larger string (e.g. a log line) is redacted
  in-place, leaving surrounding text intact.
- Empty payloads, empty strings, and records with no matchable content pass
  through unchanged (same type, equal value).
- `EnvValueRedactor` with an env var whose value is the empty string or absent
  contributes no pattern (it must not redact every empty string).
- Very short env values (e.g. a 1-char var) are excluded by a configurable
  minimum length so redaction does not blank out common substrings.

## Technical Context

### Affected Apps

- `flywheel` library: new `src/flywheel/redaction.py`; edits to
  `src/flywheel/audit/__init__.py` and `src/flywheel/audit/_cli.py`.
- `docs/vision.md` audit-stream section.

### Integration Points

- `flywheel.audit.stream` / `subscribe` / `attach_logger` — add the optional
  `redactor` parameter and apply it at the single yield/dispatch point.
- `flywheel.audit._cli` — policy selection, `--redact-policy` / `--raw` flags,
  stderr notice, and threading the redactor into `stream`.
- `flywheel.store_protocols.AuditRecord` — the redactor's input/output type;
  no change to the protocol itself.

### Relevant Existing Code

- `src/flywheel/audit/__init__.py:223` — `stream()` and the `_follow` /
  `_drain` machinery records flow through; the redactor wraps the yield.
- `src/flywheel/audit/__init__.py:330` — `Subscription._dispatch` is where a
  per-subscriber redactor would apply with existing error isolation.
- `src/flywheel/audit/_cli.py` — preview + `--json` rendering both read
  `record.payload`; redaction must precede both.
- `src/flywheel/store_protocols.py:129` / `:159` — `EventRecord` and
  `SdkMessageRecord` dataclasses (`payload: Mapping[str, Any]`).
- `tests/test_task_module_purity.py` — pattern to copy for a
  `tests/test_redaction_module_purity.py`.

### Designed-for extension: write-time redaction (NOT built in MVP)

A class of consumers (PCI-DSS, HIPAA, SOC2, GDPR data-minimization) cannot
persist secrets or PII in cleartext at all — read-time redaction does not serve
them because the store still holds raw bytes at rest. This feature does **not**
build write-time redaction, but it commits to making it a clean opt-in
follow-up rather than a redesign:

- The persistence seam is the invoker's per-message `on_message` callback that
  feeds `append_sdk_message` (and the harness's `append_event`). A future
  feature wires an opt-in `Redactor` there so cleartext never reaches the
  store.
- Because the `Redactor` abstraction is wiring-agnostic (FR-13), the *same*
  built-in redactors and consumer policies apply at either point.
- The trade is explicit and the consumer's to make: write-time redaction
  reduces forensic completeness (the unredacted bytes are gone) in exchange for
  no-cleartext-at-rest. Read-time (this MVP) is the forensics-first default.

Documenting this seam now is what makes the abstraction robust across the OSS
consumer spectrum without over-building the MVP.

## Decisions Log

- **Read-time, not write-time.** The store stays the complete forensic record;
  redaction is a view applied on read. This matches the exact wording in
  `vision.md:46` and `00006` ("a layer that sits on top of this stream"),
  preserves auditability, and lets different consumers apply different
  policies. Write-time redaction would destroy data irrecoverably and is
  rejected.
- **Pure module.** Redaction lives in its own module with the same purity
  discipline as `task`/`lifecycle` so it is trivially testable and reusable.
  The one impurity — reading `os.environ` for `EnvValueRedactor` — is confined
  to that redactor's constructor (the CLI/consumer seeds it), keeping the core
  transform pure.
- **CLI redacts by default.** The operator CLI is the most likely place a
  human pastes output somewhere public, so the safe default is redacted with an
  explicit `--raw` escape hatch. The programmatic `stream` default stays
  verbatim to avoid breaking embedded consumers.
- **One-way stable tokens via keyed HMAC.** Correlation without exposure; no
  reversibility, no key escrow.
- **One abstraction, two wiring points.** Robustness across the OSS consumer
  spectrum comes from a single wiring-agnostic `Redactor` that the MVP applies
  at read time (forensics-first default) and a follow-up can apply at the
  persistence seam (compliance-first opt-in). Read-time is correct as the
  default; write-time is the consumer's deliberate, documented trade. We design
  the seam now and build only read-time. (Decided 2026-06-02 after weighing OSS
  consumers with no-cleartext-at-rest compliance needs.)
- **Pluggable + verifiable.** Custom policies load by dotted path so consumers
  configure rather than fork; a `--dry-run` coverage report makes "best-effort"
  redaction checkable instead of silently lossy.

## Open Questions

1. Should `--raw` require an explicit confirmation or env opt-in (e.g.
   `FLYWHEEL_AUDIT_ALLOW_RAW=1`) to make accidental verbatim dumps harder, or
   is a bare flag sufficient?
2. Should the salt for stable tokens default to a per-run value (so digests
   correlate within a run but not across runs) or be fully operator-supplied?
3. Which named policies beyond `default` are worth shipping in the MVP
   (e.g. `strict` = default + tool-denylist for file-reading tools)?

## Next Steps

- Confirm the open questions, then decompose into tasks via the `/task` skill
  (suggested cut: (1) wiring-agnostic redaction core + `Redactor` protocol +
  purity test; (2) built-in redactors + `default_policy` + stable tokens;
  (3) `stream`/`subscribe`/`attach_logger` read-time integration; (4) CLI flags
  — `--redact` / `--redact-policy` dotted-path loading / `--raw` / `--dry-run`
  coverage + stderr notice; (5) vision.md doc update incl. the write-time seam).
- Write-time (no-cleartext-at-rest) redaction is a separate follow-up spec that
  reuses this feature's `Redactor`; not part of this work.

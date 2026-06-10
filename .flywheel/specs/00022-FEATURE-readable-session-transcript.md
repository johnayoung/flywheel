# Feature: Readable Session Transcript

## Summary
Make the task-detail session screen's transcript readable. Agent prose renders as full multi-line text, tool calls/results become scannable one-liners with outcome markers, known lifecycle events render as short human phrases instead of raw JSON, and the stream gains block timestamps, turn spacing, and a re-tuned style hierarchy.

## Background
Clicking into a task in the `fw` console (`SessionScreen`) shows the merged audit stream, but every record is flattened to a single line: agent text is newline-stripped and truncated at 240 chars, tool results are dim truncated blobs, and harness events dump compact sorted-key JSON. Operators cannot follow what the agent is doing or saying. All data needed for better rendering already exists on `TranscriptEntry` / the audit records; this is a presentation-layer change in the classifier and renderer.

## Scope

### In Scope
- Multi-line rendering of agent text (`EntryKind.AGENT_TEXT`) with original line breaks, wrapped to width
- Per-tool smart argument summaries on `TOOL_CALL` lines (most informative arg per tool)
- `TOOL_RESULT` lines collapsed to a `↳` outcome line: brief summary on success, capped multi-line detail on error
- Per-kind human formatters for known harness event kinds; unknown kinds keep the JSON digest
- Dim `HH:MM:SS` timestamp on the first line of each block (turn)
- Blank line between blocks when the speaker/kind group changes
- Style-map re-tune: agent text visually dominant, errors undimmed, telemetry pushed back

### Out of Scope
- The core `flywheel_core.audit` CLI line format (it is a wire format; labels are load-bearing)
- Hiding/suppressing any record kind (no information loss; everything still renders)
- Iteration boundary markers/rules
- Expand/collapse interactivity, keybindings, or any new UI state
- Pairing tool_use ids with results to merge call+result into one line
- Any change to event emission, storage schema, redaction, or steering paths

## Requirements

### Functional Requirements
1. **FR-1 Multi-line agent text**: `AGENT_TEXT` entries render with original line breaks preserved (no `_short()` newline-stripping), wrapped by the widget; no truncation cap.
   - Acceptance: a multi-paragraph assistant text block renders with its paragraphs intact in the transcript; no `…` suffix on prose of any length.
2. **FR-2 Smart tool-call summaries**: `TOOL_CALL` body shows the tool name plus its most informative argument via a per-tool map (e.g. `file_path` for Edit/Read/Write, `command` for Bash, `pattern` for Grep/Glob, `prompt`-elided for Agent); tools without a mapping fall back to the current first-two-keys form.
   - Acceptance: an Edit call renders `tool(Edit)  <file_path>`; a Bash call renders `tool(Bash)  <command>`; an unmapped tool keeps `k=v, k=v` form.
3. **FR-3 Tool-result outcome lines**: `TOOL_RESULT` renders as an indented `↳` line. Success: `ok` plus a brief content hint (first line or line count). Error (`is_error` true): styled red, content rendered multi-line up to 10 lines with `… +N more lines` beyond.
   - Acceptance: a successful Bash result renders one `↳ ok · <first line>` line; an erroring result with a 30-line traceback shows 10 lines plus `… +20 more lines`.
4. **FR-4 Humanized lifecycle events**: known harness event kinds (at minimum: `harness.iteration_completed`, `harness.awaiting_approval`, `harness.control_command_applied`, `harness.control_command_failed`, attempt/status transition events) render as a short human phrase built from payload fields (e.g. `iteration 3 · 1.2k tokens`). Unknown kinds keep the existing `kind + JSON digest` rendering unchanged.
   - Acceptance: `harness.iteration_completed {"iteration":3,"usage":{"total_tokens":1200}}` renders as a phrase containing `iteration 3` and a token count, no braces; an unrecognized kind still shows its JSON digest.
5. **FR-5 Block timestamps**: the first line of each block carries a dim `HH:MM:SS` prefix from the record's `ts`; continuation lines do not.
   - Acceptance: an agent block followed by a tool call shows exactly two timestamps, one per block start.
6. **FR-6 Turn spacing**: a blank line is inserted between consecutive blocks when the block group changes (agent prose / tool activity / operator / lifecycle), not between a tool call and its own result line.
   - Acceptance: agent text followed by a tool call has a blank line between them; a tool call followed by its `↳` result does not.
7. **FR-7 Style hierarchy**: errors (`control_command_failed`, error tool results, failed lifecycle states) are never double-dimmed and use an error style; humanized lifecycle lines stay dim; agent text uses the default (non-dim) body style.
   - Acceptance: style map has no entry where an error-class kind maps to `dim` body + `dim` header; error tool results carry a red style.

### Non-Functional Requirements
- **Performance**: rendering stays compatible with the existing ~250ms poll loop; classification remains a pure per-record transform (no cross-record buffering beyond what blank-line/timestamp grouping needs, which may compare against the previous entry only).
- **Security**: redaction continues to run before classification, unchanged; multi-line rendering must not bypass the redactor (it already operates on payloads pre-classification).
- **UX**: no new keybindings, settings, or UI state; the screen behaves identically except for line content/layout.

## Behavior Specification

### Happy Path
1. Operator presses Enter on a dashboard row; `SessionScreen` opens and drains history.
2. Transcript shows timestamped blocks: full agent prose, `tool(Name)  <key arg>` lines each followed by an indented `↳ ok · …` result, dim human phrases for harness events, blank lines between turns.
3. New records appended by the poll loop follow the same formatting, including correct spacing relative to the previous block.

### Error Handling
| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| Tool result with `is_error` | Red `↳ error` line, content up to 10 lines + `… +N more lines` |
| `harness.control_command_failed` | Humanized phrase including `error_type: message`, error style, never dimmed |
| Known event kind with missing/odd payload fields | Formatter falls back to the JSON digest for that record (never raises) |
| Non-JSON-serializable payload | Existing `_payload_digest` fallback unchanged |

### Edge Cases
| Case | Expected Behavior |
| ---- | ----------------- |
| Agent text containing `<!-- LOOP_STATUS -->` envelope or markdown | Rendered verbatim as text (no markdown interpretation, no stripping) |
| Very long single-line tool command (no newlines) | Wraps at widget width; no truncation needed for display |
| Multiple text + tool_use blocks in one AssistantMessage | Sub-entries keep record order; spacing rule treats consecutive tool_use blocks as one tool-activity group |
| First record on screen | No leading blank line |
| Tool result arriving without a visible preceding call (history truncation/redaction) | `↳` line still renders standalone; no pairing assumed |
| Empty agent text block | Entry skipped or rendered header-only, never a stray blank block |

## Technical Context

### Affected Apps
- `flywheel` (packages/flywheel): classifier and renderer changes only

### Integration Points
- `TranscriptTailer.fetch()` output consumed by `SessionScreen` — `TranscriptEntry` may need a multi-line-capable body or a structured body field; pending-command reconciliation (`control_command_id` matching) must keep working
- Redaction (`flywheel_core.redaction`) runs upstream, unchanged

### Relevant Existing Code
- `packages/flywheel/src/flywheel/_session.py` — `EntryKind`, `TranscriptEntry`, `_classify_*`, `_short`, `_summarise_tool_input`, `_payload_digest` (the classification layer; most changes land here)
- `packages/flywheel/src/flywheel/_session_screen.py` — `render_entry_text`, `_HEADER_STYLES` / `_BODY_STYLES`, transcript append path (the rendering layer)
- `packages/flywheel/tests/` — existing classifier/screen tests to extend

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Target pain points | All four: agent text, lifecycle noise, tool clutter, visual structure | All selected by operator; each is a small presentation change |
| Agent prose | Full multi-line, uncapped | Core content of the session; scrollback handles length |
| Lifecycle events | Humanize known kinds, JSON digest for unknown | Readability without information loss or a suppression list |
| Tool lines | Smarter one-liners (separate call + `↳` result lines) | Compact and scannable without invasive call/result pairing |
| Structure | Blank line between turns + block timestamps + style re-tune; no iteration markers | Chosen subset; iteration markers explicitly skipped |
| Timestamp placement | Block starts only, dim HH:MM:SS | Time context without a gutter on every wrapped line |
| Error result detail | Capped multi-line (10 lines + `… +N more lines`) | Diagnosable without tracebacks flooding the scroll |
| Audit CLI | Untouched | Its line format is a wire format; labels are load-bearing |
| Loop-path coverage | Not required | Presentation-only; trips no Trigger Set signal (no lifecycle, schema, grader, store-protocol, or control-command change) |

## Open Questions
None.

## Next Steps
Run `/task 00022-FEATURE-readable-session-transcript` to generate implementation tasks from this spec.

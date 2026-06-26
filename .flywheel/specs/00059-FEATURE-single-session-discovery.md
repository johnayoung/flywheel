# Feature: Single-session discovery (tier subagents, not per-tier sessions)

## Outcome
Autopilot's per-cycle discovery runs as **one** SDK session that dispatches the
tier-relevance agents as **subagents** (`ClaudeAgentOptions.agents`), instead of
opening one independent `ClaudeSDKClient` session per tier. One session means one
`claude` process and one MCP-server startup per cycle rather than eleven, with the
repo read once at the session boundary and the tier work fanned out as isolated
subagent contexts. Discovery's observable contract is unchanged: it still returns
exactly one `TierVerdict` per tier (1–11), each verdict still carries the same
relevance + findings JSON, and a tier whose agent fails still yields a
`(not relevant, error-reason)` verdict. The win is purely in the execution model —
~11× fewer processes and MCP boots per cycle, a far smaller resource spike, and the
elimination of the redundant per-session MCP-server churn observed in production.

## Background
Today `run_discovery` (`packages/flywheel-orchestrator/src/flywheel_orchestrator/_autopilot.py:633`)
does `asyncio.gather` over 11 calls to `build_repo_invoker`, and each call routes
through `invoke_iteration_with_client`, which "opens a persistent client session"
on a fresh `ClaudeSDKClient`. So every discovery cycle spawns **11 independent
`claude` subprocesses**, and each one boots its own MCP servers — a live run in
`infrared-rust` showed every session starting its own `playwright-mcp` and `proto`
servers, 11× per cycle, forever on the daemon's interval. This is finding **P1**
from the 2026-06-26 autopilot performance investigation (see memory
`project_autopilot_perf_and_shutdown`): an uncapped session fan-out whose cost is
process/MCP/cold-start overhead, not the relevance reasoning itself.

The installed Agent SDK (`claude-agent-sdk` 0.2.83) supports programmatic
subagents: `ClaudeAgentOptions.agents` is `dict[str, AgentDefinition]`, each
`AgentDefinition` carries its own `model`, `maxTurns`, `effort`, `tools`,
`mcpServers`, and `permissionMode` (camelCase). The orchestrator agent dispatches a
subagent via the built-in `Agent` tool (renamed from `Task` in Claude Code
v2.1.63; both names appear in the stream), and subagents run **inside the same
session/process** — no subprocess spawn, shared MCP startup — each in a fresh,
isolated context window. The decisive constraint: a subagent returns **only its
final assistant message** as the `Agent` tool result, and the parent "may summarize
it" — there is **no structured-data contract** for what the parent then says. So the
verdict JSON must be read from the subagent's tool-result block in the message
stream, never from the parent's prose.

Sources (verified via official docs, 2026-06-26):
`code.claude.com/docs/en/agent-sdk/subagents.md`,
`code.claude.com/docs/en/agent-sdk/python.md`.

## Scope
### In scope
- A production discovery invoker that builds **one** `ClaudeSDKClient` session with
  the 11 tier agents registered in `options.agents`, instructs the orchestrator to
  dispatch all relevant-to-evaluate tiers, and returns the 11 `TierVerdict`s.
- A verdict collector that parses each tier's verdict from the **`Agent`
  tool-result block** in the streamed messages (keyed to the subagent name / dispatch),
  not from the orchestrator's final text. Both `"Task"` and `"Agent"` tool names
  accepted (v2.1.63 rename).
- Tier subagents declared with **`mcpServers=[]`** (no MCP servers — kills the
  redundant `playwright`/`proto` boots) and a cheap budget (`effort`/`maxTurns`/
  `model`) appropriate to a relevance triage, read-only repo `tools`.
- Preservation of the existing seam: `run_discovery` / `discover_tier` keep accepting
  a scripted `AutopilotInvoker` so tests run with no SDK and no live model; the
  subagent path is the production default only.
- A de-risk spike (first task) proving flywheel's message-stream draining surfaces
  the per-subagent `Agent` tool-result content distinctly enough to map each result
  back to its tier and parse the verdict JSON.

### Out of scope
- The **P2 relevance cache** (skip discovery when repo HEAD is unchanged) and the
  cheap structural pre-prune of irrelevant tiers — separate spec; this one only
  changes the *execution model*, not *what gets investigated*.
- Authoring (still serial — P4) and cold-start sharing (P5).
- The cancellable-cycle graceful-shutdown change (the force-kill safety net already
  shipped in `36a0622`).
- Any change to the tier model, weights, scoring, or the integrity/landing rules.

### Must not regress
- **The 11-verdicts-always invariant**: discovery returns exactly one verdict per
  tier; a failed/timed-out tier yields `(not relevant, error-reason)`, never a drop.
- **Per-codebase, agent-judged relevance** (00058 D-1): tiers are judged by an agent
  reading *this* repo, not by coded detectors.
- **The integrity line**: discovery never decides its own work succeeded; emitted
  tasks still land only on out-of-band grades.
- **The scripted-invoker test seam**: existing discovery/daemon tests keep passing
  with injected verdicts and no SDK.
- Activation, daemon-loop idle/never-exit, and the activity surface (00058, 00059
  activity work) are untouched.

## Success Criteria
1. One discovery cycle spawns **exactly one** `claude` process for discovery, not 11
   (assert against the process model / a counting test seam).
2. MCP servers boot **once** per discovery cycle, not once per tier (tier subagents
   declare no MCP servers; verified by the spawn options).
3. `run_discovery` still returns 11 `TierVerdict`s in tier order, parsed by the same
   JSON contract, for a scripted run and a live run.
4. A tier subagent that raises / returns malformed output still produces a
   `(not relevant, error-reason)` verdict — the run never returns fewer than 11.
5. Tier verdicts are sourced from the subagent **tool-result blocks**, proven by a
   test where the orchestrator's final prose disagrees with / omits the per-tier JSON
   yet the parsed verdicts are correct.
6. The scripted `AutopilotInvoker` seam still drives discovery with no SDK import and
   no live model (purity/extra-free path intact).
7. Blind discrimination (held-out style, per repo conventions): on a fixture repo
   where tiers A,B are relevant and C is not, discovery yields the correct relevant /
   not-relevant split via the subagent path.

## Decomposition Hint (for /fw-plan)
1. **Spike**: confirm the SDK message stream exposes per-subagent `Agent` tool-result
   content that flywheel can map to a tier and parse. Record findings; if the stream
   does not distinguish results cleanly, fall back design = orchestrator emits a
   single aggregated JSON of all 11 verdicts as its final message (parsed once).
2. Build the single-session tier-subagent discovery invoker (`options.agents`,
   `mcpServers=[]`, cheap budget, `Agent` in `allowed_tools`).
3. Implement the verdict collector (tool-result blocks → `TierVerdict`s, both tool
   names, 11-verdicts-always with error fallback).
4. Wire it as the production default behind the existing `discovery_invoker` seam in
   `run_refill_pass`; keep scripted-seam parity.
5. e2e + resource proof: process/MCP-boot count assertions (criteria 1–2) and the
   blind discrimination (criterion 7).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Discovery is one session with tier subagents, not per-tier sessions  (Status: Accepted)
The 11-session fan-out is an artifact of reusing the one-call-one-session invoker
seam, not a requirement. Subagents in a single session give identical isolation per
tier (fresh context each) while collapsing 11 processes + 11 MCP boots into one.

### D-2: Tier verdicts are read from the `Agent` tool-result, never the parent's summary  (Status: Accepted)
The SDK guarantees only the subagent's final message returns verbatim as the tool
result; the parent may re-summarize. A strict `TierVerdict` JSON contract therefore
must bind to the tool-result block, not the orchestrator's prose. (If the spike shows
the stream cannot key results to tiers, supersede with the aggregated-final-JSON
fallback.)

### D-3: Tier subagents declare no MCP servers and a cheap budget  (Status: Accepted)
A relevance triage needs to read the repo, not drive a browser. `mcpServers=[]` plus
a bounded `effort`/`maxTurns`/`model` removes the redundant `playwright`/`proto`
boots seen in production and caps per-tier cost.

### D-4: The scripted-invoker seam stays; subagents are the production path only  (Status: Accepted)
Tests must keep driving discovery deterministically with no SDK. The subagent
machinery lives behind `build_repo_invoker`'s production default, exactly as the
current SDK session does.

## Open Questions (accepted gaps)
- Does flywheel's existing message draining (`flywheel_core.invoker`) surface the
  `Agent` tool-result content blocks to the consumer, or only the final
  `ResultMessage`? Resolved by the spike (task 1); drives D-2 vs. its fallback.
- Should the orchestrator be a real reasoning parent (decides which tiers to dispatch)
  or a deterministic dispatcher driven by a fixed `initialPrompt`? Leaning
  deterministic for cost/repeatability; confirm in planning.
- Whether to fold the P2 cheap pre-prune in here once the single-session shape exists
  (the parent is the natural place to triage before dispatching deep subagents).

## Next Steps
Run `/fw-plan` against this spec to produce the task DAG (spike → invoker → collector
→ wire-in → proof). The spike (task 1) gates D-2; do it first.
